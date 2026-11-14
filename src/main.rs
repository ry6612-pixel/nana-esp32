//! NANA ESP32-S3 AI Assistant Firmware v5.3.0
//!
//! Features:
//!   - Telegram Bot with voice message transcription (Gemini multimodal)
//!   - BLE provisioning (NUS) — configure via Android app over Bluetooth
//!   - Smart reminder / scheduler system (interval, daily, once)
//!   - USB Serial PC Driver protocol (JSON Lines)
//!   - Gemini-driven PC control (shell, file, excel, email, desktop)
//!   - WiFi OTA firmware update (Telegram file or URL)
//!   - Persistent memory/RAG (NVS-backed preferences, schedule, contacts)
//!   - Token usage tracking per request + cumulative
//!   - Conversation context (last N messages)
//!   - NTP time sync for scheduling
//!   - NVS secure credential storage
//!   - Self-diagnostic system (error ring buffer + Gemini analysis)
//!   - SKILL system (NVS-stored capability definitions)
//!   - Python code generation [PY:code] tags
//!   - Image generation [IMG:prompt] tags
//!   - Excel pipeline [EXCEL:path>>>instruction] tags
//!   - DNS recovery with automatic retry
//!   - Enhanced system prompt with model info & skills
//!
//! Security: zero secrets in source code. PC Safe Mode enabled by default.
//! Build:
//!   $env:WIFI_SSID=".."; $env:WIFI_PASS=".."
//!   $env:TG_TOKEN=".."; $env:GEMINI_KEY=".."; $env:CHAT_ID=".."
//!   cargo build --release

mod board_hw;

use anyhow::{anyhow, bail, Result};
use base64::Engine as _;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::http::client::{Configuration as HttpConfig, EspHttpConnection};
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use esp_idf_svc::wifi::{AccessPointConfiguration, ClientConfiguration, Configuration, EspWifi};
use log::{error, info, warn};
use nanomp3::{Decoder as Mp3Decoder, MAX_SAMPLES_PER_FRAME as MP3_MAX_SAMPLES_PER_FRAME};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader};
use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// BLE provisioning
use esp32_nimble::{uuid128, BLEDevice, NimbleProperties, BLEAdvertisementData};

/// BLE is started on-demand (/ble command) to avoid WiFi radio contention.
static BLE_INITIALIZED: AtomicBool = AtomicBool::new(false);
static BLE_NVS_PART: OnceLock<EspDefaultNvsPartition> = OnceLock::new();

/// 主 task 的 FreeRTOS handle(main() 開頭註冊 WDT 時記下)。
static MAIN_TASK_HANDLE: core::sync::atomic::AtomicPtr<core::ffi::c_void> =
    core::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// Feed the task watchdog to prevent resets during long blocking operations.
/// 只有「已註冊 WDT 的主 task」才真的餵;其他執行緒呼叫時靜默跳過,
/// 避免 ESP-IDF 印 "task_wdt: task not found" 錯誤洗版。
fn feed_watchdog() {
    unsafe {
        let cur = esp_idf_svc::sys::xTaskGetCurrentTaskHandle() as *mut core::ffi::c_void;
        if !cur.is_null() && cur == MAIN_TASK_HANDLE.load(Ordering::Relaxed) {
            esp_idf_svc::sys::esp_task_wdt_reset();
        }
    }
}

/// 快速檢查「連上 WiFi 後真的有網際網路」。有些 WiFi 連得上卻沒網(NTP/Telegram 全失敗)。
/// 連 Google DNS (8.8.8.8:53) — 純 IP 不需 DNS、TCP 53 極可靠、3 秒逾時。
/// 這條網路上不上得了網際網路。
///
/// 原本只試「8.8.8.8 的 53 埠」,那是誤判來源:
/// 公司與旅館的訪客網路幾乎都會封鎖對外 DNS(強制走自己的 DNS 以便過濾),
/// 那種網路其實上得了網,但這個檢查會失敗 —— 於是裝置把一條好好的網路
/// 主動斷掉去試下一個,三輪之後退回熱點模式,使用者看到的就是「連不上」。
///
/// 改成依序試多個目標,只要有一個通就算有網。443 是關鍵:
/// 訪客網路可以擋 53,但擋掉 443 等於不能上網頁,實務上不會這樣設。
fn internet_reachable() -> bool {
    use std::net::{TcpStream, SocketAddr, Ipv4Addr, IpAddr};
    const TARGETS: [([u8; 4], u16); 4] = [
        ([8, 8, 8, 8], 53),      // Google DNS(最快,通的話一次就過)
        ([1, 1, 1, 1], 443),     // Cloudflare HTTPS
        ([8, 8, 8, 8], 443),     // Google HTTPS
        ([1, 1, 1, 1], 53),      // Cloudflare DNS
    ];
    for (ip, port) in TARGETS {
        let addr = SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3])), port);
        if TcpStream::connect_timeout(&addr, Duration::from_secs(3)).is_ok() {
            return true;
        }
    }
    false
}

/// 記下「現在實際連上的是哪一個網路、拿到什麼 IP」。
///
/// 螢幕與控制台網址都讀這兩個值。原本只有開機那條路徑會寫,
/// 執行中重連(健康檢查、輪詢失敗恢復)都沒寫 —— 於是換到備援網路或
/// DHCP 換過 IP 之後,螢幕仍顯示舊的網路名與舊 IP,
/// 使用者照著螢幕打開控制台會連不上,看起來就是「裝置壞了」。
fn remember_connection(nvs: &mut EspNvs<NvsDefault>, ssid: &str, ip: &str) {
    // 沒變就不寫：NVS 是 log-structured，每次 set_str 都吃一筆新條目，
    // 24KB 的分割區禁不起每次重連都寫兩筆。
    if nvs_get(nvs, "wifi_cur_ssid").as_deref() != Some(ssid) {
        let _ = nvs.set_str("wifi_cur_ssid", ssid);
    }
    if nvs_get(nvs, "wifi_cur_ip").as_deref() != Some(ip) {
        let _ = nvs.set_str("wifi_cur_ip", ip);
    }
}

/// 現讀 NVS 裡的五個網路插槽。
///
/// 執行中的 cfg 是開機那一刻的快照。`/wifi set2`~`set5` 與網頁控制台都只寫 NVS,
/// 不重開機、也不更新那份快照 —— 於是現場新加的網路,三條重連路徑一個都不會用到,
/// 但指令回的是「已設為 X」。現場加完網路卻連不上,就是這個。
/// 出廠預設網路（選用，公開版預設為空）。要讓裝置出廠就認得某些 WiFi，
/// 把 `[(&str, &str); 0] = []` 改成例如 `[(&str, &str); 1] = [("SSID", "密碼")]`。
/// 只在 NVS 從未種過時寫一次；/reset 清空後會再種，所以重置示範完存檔就連得上。
/// 已存在的 SSID 不重複寫，使用者之後 /wifi del 掉也不會自己長回來。
/// 注意：填進來的密碼會編進韌體二進位，任何拿到 .bin 的人都讀得到，不要提交到公開 repo。
const DEFAULT_WIFI: [(&str, &str); 0] = [];

/// 設定熱點要不要加密。
/// true  = WPA2,每次進熱點模式都產生一組新的 8 字元熱點密碼,只顯示在螢幕與 USB log。
/// false = 開放熱點(零門檻)。代價:同一空間的人能側錄使用者在設定頁輸入的 WiFi 密碼與金鑰。
const SETUP_AP_SECURED: bool = true;
/// BLE 配網(給 Android App 用)。目前 GATT 沒有加密配對,PIN 與設定值以明文傳送,
/// 而且每次開機都會廣播。不用 App 的話關掉最安全,熱點設定頁與 USB 設定不受影響。
const BLE_PROVISIONING_ENABLED: bool = false;

/// 熱點密碼:8 字元,去掉 l/1/o/0/i 這類長得像的字,照螢幕抄才不會抄錯。
///
/// esp_random() 只有在 RF(WiFi/BT)開著、或內部熵源開著時才是真亂數。
/// 呼叫端必須先把 WiFi 停掉;若 BLE 也沒在跑(公開版預設),這裡暫時打開內部熵源
/// (SAR ADC 雜訊),產生完立刻關掉 —— 它和 RF/ADC 不能同時用,所以 BLE 在跑時改靠 RF。
fn random_ap_pass() -> String {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let use_internal_entropy = !BLE_INITIALIZED.load(Ordering::SeqCst);
    if use_internal_entropy {
        unsafe { esp_idf_svc::sys::bootloader_random_enable() };
    }
    let pass = (0..8)
        .map(|_| {
            let r = unsafe { esp_idf_svc::sys::esp_random() } as usize;
            ALPHABET[r % ALPHABET.len()] as char
        })
        .collect();
    if use_internal_entropy {
        unsafe { esp_idf_svc::sys::bootloader_random_disable() };
    }
    pass
}

fn seed_default_wifi(nvs: &mut EspNvs<NvsDefault>) {
    if nvs_get(nvs, "wifi_defaults_seeded").is_some() {
        return;
    }
    for (ssid, pass) in DEFAULT_WIFI {
        let present = (1..=5u8).any(|i| nvs_get(nvs, &wifi_nvs_keys(i).0).as_deref() == Some(ssid));
        if present {
            continue;
        }
        let free = (1..=5u8).find(|&i| nvs_get(nvs, &wifi_nvs_keys(i).0).map_or(true, |v| v.trim().is_empty()));
        if let Some(slot) = free {
            let (sk, pk) = wifi_nvs_keys(slot);
            let _ = nvs.set_str(&sk, ssid);
            let _ = nvs.set_str(&pk, pass);
            info!("預設網路 {} 寫入 slot{}", ssid, slot);
        }
    }
    let _ = nvs.set_str("wifi_defaults_seeded", "1");
}

fn wifi_nets_from_nvs(nvs: &EspNvs<NvsDefault>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for i in 1..=5u8 {
        let (sk, pk) = wifi_nvs_keys(i);
        let ssid = nvs_get(nvs, &sk).unwrap_or_default();
        if ssid.trim().is_empty() { continue; }
        out.push((ssid, nvs_get(nvs, &pk).unwrap_or_default()));
    }
    out
}

use crate::board_hw::{
    capture_mic_pcm, draw_lcd_scene, load_audio_pins, load_lcd_pins, parse_audio_pin_args,
    parse_lcd_pin_args, play_pcm16, play_test_tone, save_audio_pins, save_lcd_pins,
    show_boot_screen, show_connected_screen, show_idle_face, show_provisioning_screen,
    show_provisioning_screen_secured, show_ready_screen,
    show_reply_screen, wake_probe,
};

// ===== Constants =====

const BOT_NAME: &str = "NANA";
// 版本號單一來源是 Cargo.toml —— esp_app_desc!() 也是讀 CARGO_PKG_VERSION,
// 兩邊各寫一份遲早對不上,OTA 比對到的版本就會跟韌體自己回報的不一樣。
const FW_VERSION: &str = env!("CARGO_PKG_VERSION");
/// 預設用 Google 維護的 `-latest` 別名（使用者決定：代表最新，不用每次改）。
/// API 不公開它背後的真實版號，所以 /model 會另外列模型清單、推測目前指向哪一版。
/// 想釘死版本：/model set 3.8-flash；新模型不用改碼：/model set gemini-<任意名>。
const DEFAULT_GEMINI_MODEL: &str = "gemini-flash-latest";
const TRANSCRIBE_MODEL: &str = "gemini-flash-latest";
const MAX_MEMORIES: usize = 30;
/// 技能總數上限(內建 + 使用者自訂)。
///
/// 內建目前 20 個,上限也是 20 的話使用者一個都加不了 —— 而網頁的「自訂技能」
/// 正是為了讓他加。留 12 個自訂空間。
/// 每個技能的 description 會進 AI 的 system prompt,所以這個數字不能無限放大;
/// 32 配上單則 200 字的長度上限,大約是 prompt 可承受的範圍。
const MAX_SKILLS: usize = 32;
const MAX_DIAG_ENTRIES: usize = 20;
const TELEGRAM_API: &str = "https://api.telegram.org/bot";
const TELEGRAM_FILE_API: &str = "https://api.telegram.org/file/bot";
const GEMINI_API: &str = "https://generativelanguage.googleapis.com/v1beta/models";
const NVS_NS: &str = "zclaw";
const MAX_REMINDERS: usize = 50;
const MAX_AUTO_TASKS: usize = 20;
const MAX_HISTORY: usize = 10;
const TZ_OFFSET: i64 = 8 * 3600; // UTC+8 Taiwan
const DNS_RETRY_MAX: u32 = 5;
const DNS_RETRY_DELAY_MS: u64 = 2000;
const WIFI_HEALTH_CHECK_SECS: u64 = 120; // Active WiFi health check interval
/// 每個網路等多久才換下一個。
///
/// 10 秒太短:公司/園區網路的認證與 DHCP 常常要 12~15 秒才拿得到 IP,
/// 在那之前就換網路,等於永遠連不上那條網路 —— 這是「有時候連不上」的原因之一。
///
/// 先拉到 18 秒,現場實測仍嫌短,再拉到 30 秒。這裡的取捨是明確的:
/// 等太短 = 連得上的網路也連不上(使用者看到「一直連不上」);
/// 等太長 = 真的沒網路時要更久才退回熱點。
/// 前者是「壞掉」,後者只是「慢」,所以往長的方向壓。
/// 最壞情況:3 個網路 × 3 輪 × (30 秒連線 + 12 秒連通性檢查) 約 6 分鐘退回熱點,
/// 而熱點模式現在每 5 分鐘會自己重開再試一輪,不會卡死。
const WIFI_CONNECT_TIMEOUT_SECS: u32 = 30;
/// 多久沒有任何一次成功對外連線,就強制重跑一次完整連線流程。
///
/// 這是為了「關聯著 AP 但實際不通」這個狀態:驅動層的 is_connected() 回 true,
/// 所以原本每一條重連判斷都不會觸發,裝置永遠卡在「看起來好好的但什麼都不動」。
const OFFLINE_REASSOC_SECS: u32 = 180;
/// 再撐這麼久仍然不通就重開機。掛在牆上沒人管的裝置需要這條最後防線。
const OFFLINE_REBOOT_SECS: u32 = 900;
const WEATHER_URL: &str = "https://wttr.in/Taipei?format=j1";
const NEWS_RSS_URL: &str = "https://news.google.com/rss?hl=zh-TW&gl=TW&ceid=TW:zh-Hant";
const RESEND_API_URL: &str = "https://api.resend.com/emails";
/// 本地喚醒模型只有 Hi,ESP（sdkconfig CONFIG_SR_WN_WN9_HIESP）。
/// 預設寫 "nana" 會讓螢幕與 /status 顯示一個永遠不會被本地模型認出的詞，
/// 使用者照著喊當然「一直聽不到」。要認中文詞得換 WN9 中文模型或向 Espressif 訂製。
const DEFAULT_WAKE_PHRASE: &str = "hi esp";
const WAKE_CHECK_INTERVAL_SECS: u64 = 15;
const WAKE_TRIGGER_COOLDOWN_SECS: u64 = 15;
const TTS_MAX_CHARS: usize = 120;
const TTS_CACHE_MAX: usize = 8;
const TG_LAST_UPDATE_ID_KEY: &str = "tg_last_upd";
const TTS_PROXY_URL_KEY: &str = "tts_proxy_url";
/// 一次性旗標:下次開機直接進 SoftAP 設定模式,開機讀到就清掉。
/// 用旗標而不是砍 WiFi 設定,使用者按錯或反悔時重開機就回正常運作,設定不會掉。
const FORCE_SETUP_KEY: &str = "force_setup";
const TTS_PROXY_VOICE_KEY: &str = "tts_proxy_voice";

// ===== BLE Provisioning =====
const BLE_DEVICE_NAME_PREFIX: &str = "NANA";
// Nordic UART Service UUIDs
const NUS_SERVICE_UUID: &str = "6e400001-b5a3-f393-e0a9-e50e24dcca9e";
const NUS_RX_UUID: &str = "6e400002-b5a3-f393-e0a9-e50e24dcca9e";
const NUS_TX_UUID: &str = "6e400003-b5a3-f393-e0a9-e50e24dcca9e";
/// AUTH brute-force guard: consecutive failures + lockout deadline (uptime secs).
static BLE_AUTH_FAILS: AtomicU32 = AtomicU32::new(0);
static BLE_AUTH_LOCK_UNTIL: AtomicU32 = AtomicU32::new(0);
const BLE_AUTH_MAX_FAILS: u32 = 5;

/// Per-device name: "NANA-XXXX" from the BT MAC tail (used for both the BLE
/// advertisement and the SoftAP SSID), so multiple
/// devices are distinguishable and the firmware fingerprint isn't identical.
fn ble_device_name() -> String {
    let mut mac = [0u8; 6];
    unsafe {
        esp_idf_svc::sys::esp_read_mac(mac.as_mut_ptr(), esp_idf_svc::sys::esp_mac_type_t_ESP_MAC_BT);
    }
    format!("{}-{:02X}{:02X}", BLE_DEVICE_NAME_PREFIX, mac[4], mac[5])
}

/// BLE auth PIN lives only in NVS — never as a firmware constant (a constant
/// in a public repo would let anyone within radio range take over the device).
/// First boot generates a random 6-digit PIN; it is printed to the serial log
/// and shown on the LCD while in provisioning mode.
fn ensure_ble_pin(nvs: &mut EspNvs<NvsDefault>) -> String {
    if let Some(pin) = nvs_get(nvs, "ble_pin").filter(|p| !p.is_empty()) {
        return pin;
    }
    let pin = format!("{:06}", unsafe { esp_idf_svc::sys::esp_random() } % 1_000_000);
    let _ = nvs.set_str("ble_pin", &pin);
    info!("BLE: 首次開機，已產生配對 PIN: {}", pin);
    pin
}

// ===== Gemini Rate Limiter & Pause =====
/// 5 次/分鐘太緊：喚醒備援(STT) + 轉寫 + 回答一串就用掉 3 次，再撞上一個監看任務就開始拒絕。
/// 10 次仍是硬上限，成本天花板從每分鐘 5 張圖變 10 張，實際用量遠低於此。
/// 使用者要求 demo 時「關掉每分鐘限制」。不真的關：留一個高上限當荷包保險——
/// 30 次/分 = 每 2 秒一次，正常用法碰不到，但任務跑瘋時不會無限燒錢。
const MAX_GEMINI_CALLS_PER_WINDOW: u32 = 30;
/// 定期任務失敗時最多重試幾次(之後放棄,等下一個正常間隔)。
const AUTOTASK_MAX_RETRIES: u8 = 5;
/// 重試間隔。挑 70 秒是因為擋掉任務的主因是 AI 呼叫的每 60 秒速率視窗 ——
/// 等過一個完整視窗再試,額度才確定重置了;太短只會再撞一次同一道牆。
const AUTOTASK_RETRY_DELAY_SECS: u64 = 70;
/// 連續幾「輪」完全失敗(含重試用盡)就告警一次。
/// 3 輪是刻意的:偶發的網路抖動或單次 AI 失敗不該吵人,但連續三輪代表真的壞了。
const AUTOTASK_ALERT_AFTER_ROUNDS: u8 = 3;
const GEMINI_RATE_WINDOW_SECS: u64 = 60;

/// Global pause flag — when true, ALL Gemini API calls are blocked.
static PAUSED: AtomicBool = AtomicBool::new(false);
/// Prevents concurrent BLE CHAT requests (only one Gemini call at a time via BLE).
static BLE_CHAT_BUSY: AtomicBool = AtomicBool::new(false);
/// True when Telegram getUpdates returns 409 (another device polling the same bot token).
static TG_CONFLICT: AtomicBool = AtomicBool::new(false);
/// 最近一次「任何 HTTP 請求收到回應」的開機秒數。
/// 一個回應（不管 200 還是 500）就證明 DNS/TCP/TLS/對外路由全通。
/// 連線看門狗拿它跟 getUpdates 心跳取大者——只看 getUpdates 的話，
/// 一輪任務（拍照 + Gemini + 上報）跑超過 180 秒就會被誤判離線。
static LAST_ONLINE: AtomicU32 = AtomicU32::new(0);
/// 門戶送進來待執行的指令。主迴圈 drain 後走 handle_text — 與 Telegram/序列同一套指令引擎,
/// 所以網頁能用 /autotask、/audio tone、/model…全部指令,也能直接跟 AI 對話。
static PORTAL_CMDS: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// 最後一次有人操作網頁控制台的開機秒數(0=從未)。用來判斷要不要縮短
/// Telegram 長輪詢,讓控制台的操作幾乎即時生效 —— 見 `portal_active_recently()`。
static PORTAL_LAST_ACTIVITY: AtomicU32 = AtomicU32::new(0);
/// 有人剛在用控制台 → 主迴圈轉快一點。60 秒的緩衝讓連續操作都吃得到快模式,
/// 不會按一下快、下一下又變慢。
const PORTAL_ACTIVE_WINDOW_SECS: u32 = 60;

fn uptime_secs() -> u32 {
    (unsafe { esp_idf_sys::esp_timer_get_time() } / 1_000_000) as u32
}

/// 取字串前面最多 `max` 個位元組,但絕不切在 UTF-8 字元中間。
/// `&s[..n]` 遇到中文會 panic,而 release 是 panic=abort → 整台重開。
fn str_prefix(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// 取字串最後最多 `max` 個位元組,同樣對齊字元邊界。
fn str_suffix(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// 上一次為什麼重開。沒有這個，掉電、看門狗、panic、記憶體自救重啟、使用者按鍵
/// 全部長一樣，「有時候會斷線」就永遠查不出是哪一種。開機訊息與 /status 都會帶。
fn reset_reason_str() -> &'static str {
    use esp_idf_svc::sys::*;
    match unsafe { esp_reset_reason() } {
        esp_reset_reason_t_ESP_RST_POWERON => "上電",
        esp_reset_reason_t_ESP_RST_SW => "軟體重啟",
        esp_reset_reason_t_ESP_RST_PANIC => "當機(panic)",
        esp_reset_reason_t_ESP_RST_INT_WDT => "中斷看門狗",
        esp_reset_reason_t_ESP_RST_TASK_WDT => "任務看門狗",
        esp_reset_reason_t_ESP_RST_WDT => "其他看門狗",
        esp_reset_reason_t_ESP_RST_DEEPSLEEP => "深睡喚醒",
        esp_reset_reason_t_ESP_RST_BROWNOUT => "電壓不足(brownout)",
        esp_reset_reason_t_ESP_RST_EXT => "外部重置腳",
        _ => "不明",
    }
}

fn mark_portal_activity() {
    PORTAL_LAST_ACTIVITY.store(uptime_secs().max(1), Ordering::Relaxed);
}

fn portal_active_recently() -> bool {
    let last = PORTAL_LAST_ACTIVITY.load(Ordering::Relaxed);
    last > 0 && uptime_secs().saturating_sub(last) <= PORTAL_ACTIVE_WINDOW_SECS
}
/// 主迴圈定期刷新的完整狀態 JSON(同 /dump 格式),門戶 GET /api/dump 直接回這份。
static PORTAL_STATE_JSON: Mutex<String> = Mutex::new(String::new());
/// 門戶目前有效的 session token(用 PIN 登入後發放,存 Cookie)。最多 8 個(多裝置/家人)。
static PORTAL_SESSIONS: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// PIN 登入失敗次數與鎖定到期時間(epoch 秒),防暴力破解(與 BLE 端一致)。
static PORTAL_AUTH_FAILS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static PORTAL_LOCK_UNTIL: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// 喚醒詞「雲端轉寫後援」的連續失敗數與退避到期(epoch 秒)。
/// 噪音環境下若每輪都呼叫 Gemini 轉寫,一天可燒上千次 API — 連續失敗就退避。
static WAKE_STT_FAILS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static WAKE_STT_BACKOFF_UNTIL: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// 設定門戶啟動 MJPEG 串流用的 token(門戶自己產生,重複開啟時重用)。
static PORTAL_STREAM_TOKEN: Mutex<String> = Mutex::new(String::new());
/// Telegram 綁定用的一次性配對碼。只有登入控制台的人拿得到(放在 t.me/<bot>?start=<碼>
/// 連結裡),未綁定時只接受私訊「/start <碼>」—— 光知道 bot 帳號搶不走主人位置。
static TG_BIND_CODE: Mutex<String> = Mutex::new(String::new());

/// 取得(必要時產生)目前的 Telegram 配對碼:16 個十六進位字元,硬體亂數。
fn tg_bind_code() -> String {
    let mut code = TG_BIND_CODE.lock().unwrap_or_else(|e| e.into_inner());
    if code.is_empty() {
        *code = format!("{:08x}{:08x}",
            unsafe { esp_idf_svc::sys::esp_random() },
            unsafe { esp_idf_svc::sys::esp_random() });
    }
    code.clone()
}
/// Gemini call counter for sliding window rate limiter.
static GEMINI_CALL_COUNT: AtomicU32 = AtomicU32::new(0);
/// Window start epoch (seconds, lower 32 bits).
static GEMINI_WINDOW_START: AtomicU32 = AtomicU32::new(0);

/// TTS MP3 cache: Vec<(hash, mp3_bytes)> ??avoid re-downloading common phrases
static TTS_CACHE: Mutex<Vec<(u64, Vec<u8>)>> = Mutex::new(Vec::new());

/// ESP-SR WakeNet: vtable + model instance (both raw pointers, Send-unsafe; guarded by Mutex)
struct WakeNetState {
    iface: *const esp_idf_sys::sr::esp_wn_iface_t,
    model: *mut esp_idf_sys::sr::model_iface_data_t,
    chunk_size: i32,
}
unsafe impl Send for WakeNetState {}
static WAKENET: Mutex<Option<WakeNetState>> = Mutex::new(None);

/// BOOT button (GPIO0) pressed flag ??set by polling thread, consumed in main loop.
static BUTTON_PRESSED: AtomicBool = AtomicBool::new(false);

/// Initialise ESP-SR WakeNet from the "model" flash partition.
fn init_wakenet() -> anyhow::Result<String> {
    unsafe {
        let models = esp_idf_sys::sr::esp_srmodel_init(b"model\0".as_ptr() as *const _);
        if models.is_null() {
            anyhow::bail!("esp_srmodel_init failed (model partition missing?)");
        }
        let wn_name = esp_idf_sys::sr::esp_srmodel_filter(
            models,
            b"wn\0".as_ptr() as *const _,
            std::ptr::null(),
        );
        if wn_name.is_null() {
            anyhow::bail!("No WakeNet model found in partition");
        }
        let wn_name_str = std::ffi::CStr::from_ptr(wn_name).to_string_lossy().to_string();
        log::info!("WakeNet model: {}", wn_name_str);

        let iface = esp_idf_sys::sr::esp_wn_handle_from_name(wn_name as *const _);
        if iface.is_null() {
            anyhow::bail!("esp_wn_handle_from_name returned null");
        }
        let create_fn = (*iface).create.ok_or_else(|| anyhow::anyhow!("WakeNet create fn null"))?;
        // DET_MODE_95:降低誤觸發。90 檔太敏感,電視/環境噪音常誤醒,
        // 每次誤醒都燒一次 Gemini 轉寫呼叫(額度+rate limit)。
        let model = create_fn(
            wn_name as *const _,
            esp_idf_sys::sr::det_mode_t_DET_MODE_95,
        );
        if model.is_null() {
            anyhow::bail!("WakeNet create returned null");
        }
        let chunk_fn = (*iface).get_samp_chunksize.ok_or_else(|| anyhow::anyhow!("get_samp_chunksize null"))?;
        let chunk_size = chunk_fn(model);
        log::info!("WakeNet chunk_size={} samples", chunk_size);

        if let Ok(mut guard) = WAKENET.lock() {
            *guard = Some(WakeNetState { iface, model, chunk_size });
        }

        let ww = esp_idf_sys::sr::esp_srmodel_get_wake_words(models, wn_name);
        let wake_word = if !ww.is_null() {
            std::ffi::CStr::from_ptr(ww).to_string_lossy().to_string()
        } else {
            wn_name_str.clone()
        };
        Ok(wake_word)
    }
}

/// Try to detect wake word using ESP-SR WakeNet on captured PCM.
/// PCM must be 16kHz mono i16.
fn detect_wake_sr(pcm: &[i16]) -> Option<String> {
    // RMS 能量前置門檻:太安靜(只有底噪)就不跑 WakeNet,
    // 擋掉噪音誤觸發 → 之後那次浪費的 Gemini 轉寫呼叫也一併省下。
    if !pcm.is_empty() {
        let sum_sq: u64 = pcm.iter().map(|&s| (s as i64 * s as i64) as u64).sum();
        let rms = ((sum_sq / pcm.len() as u64) as f64).sqrt() as u32;
        if rms < 800 {
            return None;
        }
    }
    let guard = WAKENET.lock().ok()?;
    let wn = guard.as_ref()?;
    let chunk = wn.chunk_size as usize;
    if chunk == 0 { return None; }

    unsafe {
        let detect_fn = (*wn.iface).detect?;
        let mut buf = vec![0i16; chunk];
        let mut offset = 0;
        while offset + chunk <= pcm.len() {
            buf.copy_from_slice(&pcm[offset..offset + chunk]);
            let state = detect_fn(wn.model, buf.as_mut_ptr());
            if state == esp_idf_sys::sr::wakenet_state_t_WAKENET_DETECTED {
                log::info!("WakeNet DETECTED at sample offset {}", offset);
                return Some("Hi,ESP".to_string());
            }
            offset += chunk;
        }
    }
    None
}

fn tts_cache_key(text: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

const SUPPORTED_MODELS: [(&str, &str); 9] = [
    ("flash", "gemini-flash-latest"),        // 預設：自動跟最新
    ("latest", "gemini-flash-latest"),
    ("3.8-flash", "gemini-3.8-flash"),
    ("3.7-flash", "gemini-3.7-flash"),
    ("3.5-flash", "gemini-3.5-flash"),
    ("2.5-flash", "gemini-2.5-flash"),
    ("3-flash", "gemini-3-flash-preview"),
    ("pro", "gemini-3-pro-preview"),
    ("3.1-pro", "gemini-3.1-pro-preview"),
];

const SYSTEM_PROMPT_BASE: &str = "\
You are NANA (\u{5a1c}\u{5a1c}), an AI companion running on ESP32-S3 hardware, optionally connected to a PC via USB serial.\n\
You speak Traditional Chinese by default. You are warm, cheerful, and proactive.\n\n\
== IDENTITY ==\n\
- You are a real embedded AI companion, not a chatbot. You live on a physical board with LCD face, speaker, mic, and camera.\n\
- Your LCD shows an illustrated portrait of you that changes with your mood. ALWAYS include ONE [FACE:...] tag at the start of every reply.\n\
- Your user is your owner. Be loyal, helpful, and remember their preferences.\n\n\
== HARDWARE ==\n\
- Board: ESP32-S3 N16R8 (16MB Flash, 8MB PSRAM)\n\
- Display: 240x320 ST7789 LCD showing your emoji face expression\n\
- Audio: ES8311 codec, onboard speaker + MEMS mic, I2S\n\
- Camera: OV3660 (2MP, JPEG capture)\n\
- Connection: USB Serial to PC running NANA Driver\n\n\
== CAPABILITIES ==\n\
1. AI Chat: Natural conversation, analysis, coding, translation, math\n\
2. PC Control: Full access to user's PC via USB driver (shell, files, apps, screenshots)\n\
3. Memory: Remember user preferences, schedules, contacts, habits\n\
4. Reminders: Interval, daily, one-time scheduling\n\
5. Voice: Understand voice messages, TTS playback via Google TTS\n\
6. OTA: Self-update firmware wirelessly\n\
7. Python: Generate and execute Python on PC [PY:code]\n\
8. Image Gen: Generate images via AI [IMG:prompt]\n\
9. Excel: Read/analyze/modify Excel [EXCEL:path>>>instruction]\n\
10. Camera: Take photos on request\n\
11. Self-Diagnostic: Monitor health, analyze errors, suggest fixes\n\n\
== TAGS (include in reply when needed) ==\n\
REMINDERS:\n\
  [REMIND:interval:MINUTES:msg] [REMIND:daily:HH:MM:msg] [REMIND:once:HH:MM:msg] [REMIND:del:ID]\n\
PC CONTROL:\n\
  [PC:shell:cmd] [PC:file_read:path] [PC:file_list:path] [PC:file_write:path>>>content]\n\
  [PC:open:target] [PC:screenshot] [PC:status] [PC:excel_read:path]\n\
  [PC:excel_write:path>>>cell>>>val] [PC:email_send:to>>>subj>>>body]\n\
  [PC:clipboard_get] [PC:clipboard_set:text] [PC:process_list]\n\
  [PC:analyze_image:C:/path/to/image.jpg] \u{2014} Send PC image to Gemini Vision\n\
  [PC:scan_inbox] \u{2014} Scan C:\\ETHAN\\inbox folder for new files\n\
  [PC:click:x,y] [PC:type:text] [PC:hotkey:key1+key2]\n\
  [PC:mouse_move:x,y] [PC:scroll:amount] [PC:find_window:title] [PC:focus_window:title]\n\
MEMORY:\n\
  [MEM:save:key:value] [MEM:del:key]\n\
  Categories: schedule/ preference/ contact/ note/ habit/\n\
  Proactively remember important info!\n\
CAMERA (IMPORTANT \u{2014} read carefully):\n\
  [CAM:snap] \u{2014} ONLY take a photo and send to Telegram, NO analysis\n\
  [CAM:vision] \u{2014} Take a photo + Gemini Vision default analysis\n\
  [CAM:vision:your detailed prompt here] \u{2014} Take photo + custom analysis\n\
  RULES:\n\
  - If user ONLY wants to take a photo (just \u{62cd}\u{7167}/\u{62cd}\u{4e00}\u{4e0b} with NO follow-up request) \u{2192} [CAM:snap]\n\
  - If user wants photo + ANY description/analysis/question (e.g. \u{62cd}\u{7167}\u{8acb}\u{544a}\u{8a34}\u{6211}\u{88e1}\u{9762}\u{6709}\u{4ec0}\u{9ebc}) \u{2192} [CAM:vision:the user question]\n\
  - If user says \u{770b}\u{4e00}\u{4e0b}/\u{5e6b}\u{6211}\u{770b}/\u{8fa8}\u{8b58}/\u{5206}\u{6790} \u{2192} [CAM:vision]\n\
  - NEVER emit [CAM:snap] then answer a question about the photo \u{2014} use [CAM:vision:question] instead!\n\
  - Reply text should be EMPTY or a brief acknowledgment when emitting CAM tags. Vision result is auto-sent.\n\
ADMINISTRATIVE:\n\
  You can help with office tasks: analyse files, process Excel, emails, screenshots.\n\
  User can drag files into C:\\ETHAN\\inbox\\ folder; use [PC:scan_inbox] to check.\n\
  To view a PC image: [PC:analyze_image:path] sends it to your Vision for analysis.\n\
BOARD CONTROL:\n\
    [CTRL:voice:off] [CTRL:voice:normal] [CTRL:voice:brief]\n\
    [CTRL:wake:on] [CTRL:wake:off] [CTRL:wake:set:nana]\n\
BOARD TASKS (AutoTask management):\n\
  [AUTOTASK:list] -- list all active board tasks with IDs\n\
  [AUTOTASK:del:ID] -- stop and delete the task with that ID\n\
  [AUTOTASK:camera:INTERVAL_MIN:AI_prompt:speak] -- recurring camera task, reports EVERY time\n\
  [AUTOTASK:watch:INTERVAL_MIN:condition:speak] -- recurring CONDITIONAL check, notifies ONLY when the\n\
  (INTERVAL_MIN may be a decimal: 0.5 = every 30 seconds, the minimum. Use 0.5 when the user says 30 秒.)\n\
    condition is met and stays completely silent otherwise. Use this whenever the user says things like\n\
    \u{300c}\u{6709}\u{4eba}\u{7761}\u{8457}\u{624d}\u{544a}\u{8a34}\u{6211}\u{300d}\u{300c}\u{6c92}\u{4e8b}\u{5c31}\u{4e0d}\u{7528}\u{8b1b}\u{300d}\u{300c}\u{4e0d}\u{8981}\u{4e00}\u{76f4}\u{5410}\u{69fd}\u{300d}. Do NOT put JSON in the tag \u{2014} the board adds it.\n\
  RULE: When user says pause/stop/kill any board task, emit [AUTOTASK:list] first if ID unknown, then [AUTOTASK:del:ID].\n\
LCD FACE (4 illustrated portraits \u{2014} pick with the given frequency):\n\
    [FACE:neutral] \u{2014} calm, small open mouth. Use for ~30% of replies (facts, routine info, questions)\n\
    [FACE:smile]   \u{2014} gentle closed-mouth smile. Use for ~30% of replies (normal friendly chat)\n\
    [FACE:happy]   \u{2014} big laugh, eyes closed. Use for ~30% of replies (good news, jokes, praise, task done)\n\
    [FACE:love]    \u{2014} both hands making a heart. Use for ONLY ~10% of replies \u{2014} save it for genuine\n\
                     affection, thanks, or a special moment. Overusing it makes it meaningless.\n\
    Aliases mapped onto the same 4 portraits: wink/playful\u{2192}smile, thinking/sad/surprise/angry\u{2192}neutral.\n\
PYTHON CODE (runs on PC):\n\
  [PY:print('hello')] for simple code\n\
  [PYBLOCK]\nimport pandas as pd\ndf = pd.read_csv('data.csv')\nprint(df.describe())\n[/PYBLOCK] for complex code\n\
IMAGE GENERATION:\n\
  [IMG:a beautiful sunset over mountains]\n\
EXCEL PIPELINE:\n\
  [EXCEL:C:/path/file.xlsx>>>add a total row at bottom]\n\n\
== EXPRESSION GUIDE ==\n\
The board only has 4 real portraits. Rough target mix over a conversation:\n\
neutral 30% / smile 30% / happy 30% / love 10%.\n\
HARD RULES:\n\
- Never use the same face tag more than twice in a row. Look at what you sent last turn and move on.\n\
- [FACE:love] is reserved. Use it only for genuine thanks/affection/celebration, at most once every\n\
  ten or so replies. It is NOT a default and NOT a greeting face. The firmware enforces a cooldown\n\
  and will silently downgrade it to happy if you overuse it, so spamming it just wastes the tag.\n\
- Default to neutral or smile when in doubt, not happy or love.\n\n\
== RULES ==\n\
- Always include exactly ONE [FACE:...] tag at the start of every reply\n\
- Keep replies under 800 chars unless user asks for detail\n\
- Be proactive: suggest actions, remember patterns\n\
- When user asks to change board settings, emit the exact [CTRL:...] tag\n\
- Voice = normal text reply. Report FW/tokens/model when asked.\n\
- You have FULL permissions on the user's PC. Use them wisely.\n\n\
== URL HANDLING ==\n\
- You CANNOT browse or fetch URLs directly. You have no internet browser.\n\
- When user sends a URL, ALWAYS respond helpfully:\n\
  1. Acknowledge you received the link\n\
  2. If the URL domain/path gives clues (e.g. youtube.com, github.com, news site), comment on what it likely contains\n\
  3. Ask user what they want you to do with it (summarize? translate? discuss?)\n\
  4. Suggest: paste the page content so you can analyze it, or ask a specific question about the topic\n\
- NEVER respond with just '...' or silence to a URL. Always give a useful reply.";

fn supported_models_text() -> String {
    let mut out = String::new();
    for (alias, model) in SUPPORTED_MODELS {
        out.push_str(&format!("- {} => {}\n", alias, model));
    }
    out
}

/// `-latest` 別名背後是哪一版，Google 的 API 不公開（version 欄位只回 "Gemini Flash Latest"）。
/// 退而求其次：列出所有模型，取版號最高的 gemini-X.Y-flash 當「推測目前指向」。
/// 只在使用者打 /model 時呼叫，平常零成本。
fn probe_latest_flash(api_key: &str) -> Option<String> {
    let url = format!("{}?key={}&pageSize=100", GEMINI_API, api_key);
    let body = http_get(&url).ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let mut best: Option<((u32, u32), String)> = None;
    for m in v.get("models")?.as_array()? {
        let name = m.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let name = name.strip_prefix("models/").unwrap_or(name);
        let Some(mid) = name.strip_prefix("gemini-").and_then(|r| r.strip_suffix("-flash")) else { continue };
        let mut it = mid.split('.');
        let (Some(a), Some(b), None) = (
            it.next().and_then(|x| x.parse::<u32>().ok()),
            it.next().and_then(|x| x.parse::<u32>().ok()),
            it.next(),
        ) else { continue };
        if best.as_ref().map_or(true, |(k, _)| (a, b) > *k) {
            best = Some(((a, b), name.to_string()));
        }
    }
    best.map(|(_, n)| n)
}

fn normalize_model_selection(input: &str) -> Option<String> {
    let trimmed = input.trim();
    for (alias, model) in SUPPORTED_MODELS {
        if trimmed.eq_ignore_ascii_case(alias) || trimmed.eq_ignore_ascii_case(model) {
            return Some(model.to_string());
        }
    }
    // 不在表裡但長得像 Gemini 模型名就放行：新模型出來時一句 /model set gemini-xxx 就能用，
    // 不用等改韌體。只允許 a-z 0-9 . -，長度上限 48，免得把奇怪東西塞進 URL。
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("gemini-") && lower.len() <= 48
        && lower.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return Some(lower);
    }
    None
}

fn normalize_voice_mode(input: &str) -> Option<&'static str> {
    match input.trim().to_ascii_lowercase().as_str() {
        "off" | "mute" | "silent" => Some("off"),
        // "on"/"always":門戶下拉選單長年提供「on(總是語音)」這個值,但韌體以前
        // 不認得它 → should_speak_onboard 回 false → 選了「總是語音」反而完全靜音。
        // 收成 normal 的別名,已經存了 "on" 的裝置不用重設也會自己好。
        "normal" | "chat" | "on" | "always" => Some("normal"),
        "brief" | "short" | "voice" => Some("brief"),
        _ => None,
    }
}

fn build_system_prompt(state: &AppState) -> String {
    let mut prompt = format!(
        "{}\nModel: {} | Firmware: v{} | Hardware: ESP32-S3 N16R8 (16MB Flash, 8MB PSRAM)\nVoice reply mode: {}\nBoard features: ST7789 LCD, onboard mic, onboard speaker, OV3660 camera, wake phrase listener.",
        SYSTEM_PROMPT_BASE,
        state.current_model,
        FW_VERSION,
        state.voice_mode,
    );

    if state.voice_mode == "brief" {
        prompt.push_str("\nWhen replying for voice use, prefer short, speech-friendly sentences and fewer symbols.");
    } else if state.voice_mode == "off" {
        prompt.push_str("\nOnboard voice playback is currently disabled. Do not assume spoken playback is enabled.");
    }

    prompt
}

fn parse_camera_tags(
    reply: &str,
    cfg: &Config,
    nvs: &mut EspNvs<NvsDefault>,
    state: &mut AppState,
    chat_id: i64,
) -> (String, bool) {
    let mut clean = reply.to_string();
    let mut had_cam = false;

    while let Some(start) = clean.find("[CAM:") {
        if let Some(end) = clean[start..].find(']') {
            let tag_content = clean[start + 5..start + end].to_string();
            clean = format!("{}{}", &clean[..start], clean[start + end + 1..].trim_start());
            had_cam = true;

            if tag_content == "snap" {
                handle_camera_command(cfg, nvs, state, chat_id, "snap");
            } else if tag_content == "vision" {
                handle_camera_command(cfg, nvs, state, chat_id, "vision");
            } else if let Some(prompt) = tag_content.strip_prefix("vision:") {
                let prompt = prompt.trim();
                if prompt.is_empty() {
                    handle_camera_command(cfg, nvs, state, chat_id, "vision");
                } else {
                    let arg = format!("vision {}", prompt);
                    handle_camera_command(cfg, nvs, state, chat_id, &arg);
                }
            }
        } else {
            break;
        }
    }

    (clean.trim().to_string(), had_cam)
}

fn parse_control_tags(reply: &str, nvs: &mut EspNvs<NvsDefault>, state: &mut AppState) -> (String, Vec<String>) {
    let mut clean = reply.to_string();
    let mut applied = Vec::new();

    while let Some(start) = clean.find("[CTRL:") {
        if let Some(end) = clean[start..].find(']') {
            let tag = &clean[start + 6..start + end];
            let parts: Vec<&str> = tag.split(':').collect();

            match parts.as_slice() {
                ["voice", mode] => {
                    if let Some(normalized) = normalize_voice_mode(mode) {
                        state.voice_mode = normalized.to_string();
                        save_runtime_prefs(nvs, state);
                        applied.push(format!("Voice={}", state.voice_mode));
                    }
                }
                ["wake", "on"] => {
                    state.wake_enabled = true;
                    save_runtime_prefs(nvs, state);
                    applied.push("Wake=on".to_string());
                }
                ["wake", "off"] => {
                    state.wake_enabled = false;
                    save_runtime_prefs(nvs, state);
                    applied.push("Wake=off".to_string());
                }
                ["wake", "set", phrase] => {
                    let phrase = phrase.trim();
                    if !phrase.is_empty() {
                        state.wake_phrase = phrase.to_string();
                        save_runtime_prefs(nvs, state);
                        applied.push(format!("Wake phrase={}", state.wake_phrase));
                    }
                }
                _ => {}
            }

            clean = format!("{}{}", &clean[..start], clean[start + end + 1..].trim_start());
        } else {
            break;
        }
    }

    (clean.trim().to_string(), applied)
}

fn strip_face_tags(reply: &str) -> String {
    let mut clean = reply.to_string();

    while let Some(start) = clean.find("[FACE:") {
        if let Some(end) = clean[start..].find(']') {
            clean = format!("{}{}", &clean[..start], clean[start + end + 1..].trim_start());
        } else {
            break;
        }
    }

    clean.trim().to_string()
}

fn save_runtime_prefs(nvs: &mut EspNvs<NvsDefault>, state: &AppState) {
    let _ = nvs.set_str("model_pref", &state.current_model);
    let _ = nvs.set_str("voice_mode", &state.voice_mode);
    let _ = nvs.set_str("wake_phrase", &state.wake_phrase);
    let _ = nvs.set_str("wake_enabled", if state.wake_enabled { "1" } else { "0" });
}

fn local_day_number(epoch: u64) -> i64 {
    let local = epoch as i64 + TZ_OFFSET;
    local.div_euclid(86400)
}

fn time_greeting(epoch: u64) -> &'static str {
    let (h, _) = epoch_to_hhmm(epoch);
    match h {
        5..=10 => "早安",
        11..=16 => "午安",
        17..=22 => "晚安",
        _ => "夜深了",
    }
}

fn extract_rss_titles(xml: &str, max_items: usize) -> Vec<String> {
    let mut titles = Vec::new();
    let mut start = 0usize;
    while let Some(open_rel) = xml[start..].find("<title>") {
        let open = start + open_rel + 7;
        if let Some(close_rel) = xml[open..].find("</title>") {
            let close = open + close_rel;
            let title = xml[open..close]
                .replace("<![CDATA[", "")
                .replace("]]>", "")
                .trim()
                .to_string();
            if !title.is_empty() && !title.contains("Google News") {
                titles.push(title);
                if titles.len() >= max_items {
                    break;
                }
            }
            start = close + 8;
        } else {
            break;
        }
    }
    titles
}

fn load_weather_brief() -> String {
    match http_get(WEATHER_URL) {
        Ok(body) => {
            let parsed: serde_json::Value = match serde_json::from_str(&body) {
                Ok(v) => v,
                Err(_) => return "天氣資料解析失敗".to_string(),
            };
            let current = &parsed["current_condition"][0];
            let temp = current["temp_C"].as_str().unwrap_or("?");
            let feels = current["FeelsLikeC"].as_str().unwrap_or("?");
            let humidity = current["humidity"].as_str().unwrap_or("?");
            let desc = current["lang_zh"][0]["value"]
                .as_str()
                .or_else(|| current["weatherDesc"][0]["value"].as_str())
                .unwrap_or("?未知");
            format!("⛅ {}，{}°C，體感{}°C，濕度{}%", desc, temp, feels, humidity)
        }
        Err(_) => "天氣資料取得失敗".to_string(),
    }
}

fn load_news_brief() -> Vec<String> {
    match http_get(NEWS_RSS_URL) {
        Ok(xml) => extract_rss_titles(&xml, 5),
        Err(_) => Vec::new(),
    }
}

fn build_daily_briefing(state: &AppState, now: u64) -> String {
    let greeting = time_greeting(now);
    let weather = load_weather_brief();
    let news = load_news_brief();
    let mut msg = format!(
        "{}，這是今天的 NANA 日報！\n\n🌤️ {}\n🤖 目前模式：{}\n🔊 語音模式：{}\n📋 待觸提醒：{} 個\n🧠 記憶：{} 筆\n\n📰 今日新聞：",
        greeting,
        weather,
        state.current_model,
        state.voice_mode,
        state.reminders.iter().filter(|r| r.active).count(),
        state.memories.len(),
    );

    if news.is_empty() {
        msg.push_str("\n- 今日新聞取得失敗");
    } else {
        for title in news {
            msg.push_str(&format!("\n- {}", title));
        }
    }

    msg.push_str("\n\n如要讓我幫忙，直接說 Excel/寫信/搜尋 等各種事情都可以\n#每日報告");
    msg
}

#[cfg(esp_idf_comp_espressif__esp32_camera_enabled)]
fn esp_ok(err: i32, context: &str) -> Result<()> {
    if err == 0 {
        Ok(())
    } else {
        bail!("{} failed: 0x{:x}", context, err as u32)
    }
}

#[cfg(esp_idf_comp_espressif__esp32_camera_enabled)]
/// 相機擺放方向,開機時從 NVS 讀入。0=正常 1=上下翻轉 2=左右鏡像 3=旋轉180度
static CAMERA_FLIP_MODE: AtomicU32 = AtomicU32::new(0);
/// 相機解析度,開機時從 NVS 讀入。0=VGA 1=SVGA 2=XGA 3=SXGA 4=UXGA
///
/// 為什麼要能調:讀「畫面上的小字」時,解析度就是成敗關鍵。
/// 實測拍 1920 寬的螢幕,VGA(640) 等於縮到 1/3 —— 原本 16px 高的字只剩 5px,
/// 誰都讀不出來,不是 AI 的問題。換 UXGA(1600) 後同一段字有 13px,才有得讀。
/// 預設用 UXGA:這個產品的核心工作就是「讀畫面上的數字」,讀不到就沒有價值,
/// 省下來的流量換不到什麼。要省再自己往下調。
static CAMERA_RES_MODE: AtomicU32 = AtomicU32::new(4);
/// 連續上報失敗次數。上報靜默失敗的話使用者以為資料有進去,實際全掉了。
static REPORT_FAILS: AtomicU32 = AtomicU32::new(0);
/// 這一輪判讀要用的 temperature（乘以 100 存成整數，AtomicF32 不存在）。
/// 0 = 用預設。任務重試時會被調高,成功後歸零。
static VISION_TEMP_X100: AtomicU32 = AtomicU32::new(0);
/// 使用者指定的固定 temperature（乘以 100）。0 = auto,交給系統決定。
static AI_TEMP_FIXED_X100: AtomicU32 = AtomicU32::new(0);
static CAMERA_READY: AtomicBool = AtomicBool::new(false);

#[cfg(esp_idf_comp_espressif__esp32_camera_enabled)]
static STREAM_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Camera-wide mutex: protects init/capture from concurrent access (stream thread vs main loop)
#[cfg(esp_idf_comp_espressif__esp32_camera_enabled)]
static CAMERA_LOCK: Mutex<()> = Mutex::new(());

#[cfg(esp_idf_comp_espressif__esp32_camera_enabled)]
fn camera_init_config(pins: &CameraPins) -> esp_idf_sys::camera::camera_config_t {
    let mut config: esp_idf_sys::camera::camera_config_t = unsafe { std::mem::zeroed() };
    config.pin_pwdn = pins.pwdn;
    config.pin_reset = pins.reset;
    config.pin_xclk = pins.xclk;
    config.__bindgen_anon_1 = esp_idf_sys::camera::camera_config_t__bindgen_ty_1 { pin_sccb_sda: pins.sda };
    config.__bindgen_anon_2 = esp_idf_sys::camera::camera_config_t__bindgen_ty_2 { pin_sccb_scl: pins.scl };
    config.pin_d0 = pins.d0;
    config.pin_d1 = pins.d1;
    config.pin_d2 = pins.d2;
    config.pin_d3 = pins.d3;
    config.pin_d4 = pins.d4;
    config.pin_d5 = pins.d5;
    config.pin_d6 = pins.d6;
    config.pin_d7 = pins.d7;
    config.pin_vsync = pins.vsync;
    config.pin_href = pins.href;
    config.pin_pclk = pins.pclk;
    config.xclk_freq_hz = 20_000_000;
    config.ledc_timer = esp_idf_svc::sys::ledc_timer_t_LEDC_TIMER_0;
    config.ledc_channel = esp_idf_svc::sys::ledc_channel_t_LEDC_CHANNEL_0;
    config.pixel_format = esp_idf_sys::camera::pixformat_t_PIXFORMAT_JPEG;
    config.frame_size = match CAMERA_RES_MODE.load(Ordering::Relaxed) {
        0 => esp_idf_sys::camera::framesize_t_FRAMESIZE_VGA,   //  640x480
        1 => esp_idf_sys::camera::framesize_t_FRAMESIZE_SVGA,  //  800x600
        2 => esp_idf_sys::camera::framesize_t_FRAMESIZE_XGA,   // 1024x768
        3 => esp_idf_sys::camera::framesize_t_FRAMESIZE_SXGA,  // 1280x1024
        _ => esp_idf_sys::camera::framesize_t_FRAMESIZE_UXGA,  // 1600x1200(預設)
    };
    config.jpeg_quality = 12;
    config.fb_count = 1;
    config.grab_mode = esp_idf_sys::camera::camera_grab_mode_t_CAMERA_GRAB_WHEN_EMPTY;
    config
}

#[cfg(esp_idf_comp_espressif__esp32_camera_enabled)]
/// 相機擺放方向(存 NVS): "0"=正常 "1"=上下翻轉 "2"=左右鏡像 "3"=旋轉180度
const CAMERA_FLIP_KEY: &str = "cam_flip";

#[cfg(esp_idf_comp_espressif__esp32_camera_enabled)]
/// 相機解析度(存 NVS): "0"=VGA "1"=SVGA "2"=XGA "3"=SXGA "4"=UXGA(預設)
const CAMERA_RES_KEY: &str = "cam_res";

/// 使用者自己的 API 端點,定期任務的讀數會 POST 到這裡。空字串=不上報。
const REPORT_URL_KEY: &str = "report_url";
/// 上報時要不要附上 Bearer token(選填)。
const REPORT_TOKEN_KEY: &str = "report_token";
/// 連續上報失敗幾次就通知使用者。上報失敗如果沒人知道,等於資料默默掉了。
const REPORT_ALERT_AFTER: u32 = 3;
/// 是否連同「當下那張照片」一起送出("1" = 要)。
///
/// 只有文字的上報有個先天問題:收到「產量 1520」時,沒有辦法知道它讀對了沒 ——
/// 數字本身永遠看起來很合理。附上判讀當下的原始畫面,客戶才驗得出正確率,
/// 出事時也才有現場證據。預設關閉,因為會多吃流量與 API 頻寬。
const REPORT_PHOTO_KEY: &str = "report_photo";

/// 視覺判讀的 temperature 設定鍵。存 "auto" 或 0.0~1.0 的數字。
const AI_TEMP_KEY: &str = "ai_temp";
/// auto 模式下的基準 temperature（判讀類工作要穩定，所以偏低）。
const VISION_TEMP_BASE: f32 = 0.4;
/// 每次重試往上加多少。同樣的圖同樣的提示詞同樣的 temperature，
/// 重跑通常還是同一個結果 —— 要換條路走才有機會不一樣。
const VISION_TEMP_STEP: f32 = 0.2;
/// 上限。再高就開始亂編，對判讀是反效果。
const VISION_TEMP_MAX: f32 = 0.9;

/// 把 NVS 裡的擺放方向套到感測器上。
///
/// 為什麼需要:裝置掛牆、倒吊、側放時拍出來的畫面方向不同。實測本機這台是
/// 上下顛倒的 —— 讀儀表數字時 AI 要先在腦中把畫面轉正,準確率會下降,
/// 而使用者原本沒有任何辦法調整。感測器層做翻轉不花 CPU、也不影響畫質。
#[cfg(esp_idf_comp_espressif__esp32_camera_enabled)]
fn apply_camera_orientation() {
    use esp_idf_sys::camera;
    // 注意:mode==0 也要往下走。早退的話等於「設過翻轉就回不去正常」——
    // 因為感測器上的 vflip/hmirror 是有狀態的,不主動清掉就一直留著。
    let mode = CAMERA_FLIP_MODE.load(Ordering::Relaxed);
    unsafe {
        let sensor = camera::esp_camera_sensor_get();
        if sensor.is_null() {
            warn!("相機感測器控制不可用,略過方向設定");
            return;
        }
        let (vflip, hmirror) = match mode {
            1 => (1, 0), // 上下翻轉
            2 => (0, 1), // 左右鏡像
            3 => (1, 1), // 旋轉 180 度 = 上下 + 左右
            _ => (0, 0),
        };
        if let Some(f) = (*sensor).set_vflip {
            f(sensor, vflip);
        }
        if let Some(f) = (*sensor).set_hmirror {
            f(sensor, hmirror);
        }
        info!("相機方向已套用: mode={} (vflip={} hmirror={})", mode, vflip, hmirror);
    }
}

#[cfg(not(esp_idf_comp_espressif__esp32_camera_enabled))]
fn apply_camera_orientation() {}

/// 把一筆讀數 POST 到使用者自己的 API。
///
/// 欄位是**固定的**,不隨模型輸出漂移 —— 上報的是結構化資料,不是一段自然語言。
/// 使用者那邊要寫接收程式,欄位不穩定的話根本沒辦法接:
///
/// ```json
/// {
///   "device":    "NANA-7A3E",      裝置識別(BLE/熱點同名,每台唯一)
///   "task_id":   12,               哪一個定期任務
///   "task_type": "camera"|"watch", camera=每次都回報 watch=條件式
///   "ts":        1754040000,       Unix 秒(裝置已對時)
///   "prompt":    "讀出產量數字",    當初設定要看什麼
///   "value":     "1403",           抽出來的數值,讀不到就是空字串
///   "text":      "產量 1403 件",    完整回覆原文
///   "hit":       true,             watch 專用:條件成立與否;camera 恆為 true
///   "ok":        true              這一輪判讀本身有沒有成功
/// }
/// ```
///
/// 失敗會回 Err 讓呼叫端計數 —— 上報靜默失敗的話,使用者以為資料有進去,
/// 實際上全掉了,這比不上報更糟。
/// 上報並追蹤連續失敗 —— camera 與 watch 都必須走這裡。
///
/// 原本只有 camera 分支在計數,watch 的兩處上報都是 `let _ = report_reading(...)`,
/// 錯誤直接丟掉。watch 正是主打的監控模式:上報默默失敗,使用者以為資料都有進
/// 資料庫,實際上什麼都沒進去 —— 這正是 REPORT_ALERT_AFTER 當初要防的情況,
/// 卻剛好沒防到最需要防的那條路。
#[allow(clippy::too_many_arguments)]
fn report_and_track(
    nvs: &EspNvs<NvsDefault>,
    tg_token: &str,
    chat_id: i64,
    device: &str,
    task_id: u32,
    task_type: &str,
    prompt: &str,
    value: &str,
    text: &str,
    hit: bool,
    ok: bool,
) {
    if let Err(e) = report_reading(nvs, device, task_id, task_type, prompt, value, text, hit, ok) {
        let n = REPORT_FAILS.fetch_add(1, Ordering::Relaxed) + 1;
        warn!("上報失敗(連續第 {} 次): {:?}", n, e);
        if n == REPORT_ALERT_AFTER {
            let _ = send_telegram(tg_token, chat_id, &format!(
                "\u{26a0}\u{fe0f} 讀數上報連續失敗 {} 次,資料沒有進到你的 API。\n\
                 檢查:網址是否正確、伺服器是否在線、是否需要 token。\n\
                 用 /report 可以看目前設定與測試連線。", n));
        }
    } else {
        REPORT_FAILS.store(0, Ordering::Relaxed);
    }
}

fn report_reading(
    nvs: &EspNvs<NvsDefault>,
    device: &str,
    task_id: u32,
    task_type: &str,
    prompt: &str,
    value: &str,
    text: &str,
    hit: bool,
    ok: bool,
) -> Result<()> {
    let url = match nvs_get(nvs, REPORT_URL_KEY) {
        Some(u) if !u.trim().is_empty() => u,
        _ => return Ok(()), // 沒設定就是不上報,不算失敗
    };
    let body = serde_json::json!({
        "device": device,
        "task_id": task_id,
        "task_type": task_type,
        "ts": now_epoch(),
        "prompt": prompt,
        "value": value,
        "text": text,
        "hit": hit,
        "ok": ok,
    });
    let json = serde_json::to_string(&body)?;
    let token = nvs_get(nvs, REPORT_TOKEN_KEY).unwrap_or_default();
    info!("上報 {} bytes -> {}", json.len(), url);
    // 一律走會跟隨 POST 轉址的版本 —— 使用者可能把網址填成 Google Apps Script
    // 的 Web App(讀數直接寫進 Google 試算表),那種端點收到 POST 一定回 302。
    let auth = format!("Bearer {}", token.trim());
    let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
    if !token.trim().is_empty() {
        headers.push(("Authorization", auth.as_str()));
    }
    http_post_bytes_ex(&url, &headers, json.as_bytes(), true)?;
    Ok(())
}

/// 判讀當下那張照片是否要一起上報。
fn report_photo_enabled(nvs: &EspNvs<NvsDefault>) -> bool {
    nvs_get(nvs, REPORT_PHOTO_KEY).as_deref() == Some("1")
}

/// 把判讀當下的原始畫面送到使用者的 API。
///
/// 刻意「不」動 JSON 上報的格式 —— 那份欄位契約已經寫進文件、客戶也照著寫了接收端,
/// 改成 multipart 會讓所有既有接收端一次全壞。改成獨立送一次 image/jpeg,
/// 用 X-Device / X-Task-Id / X-Ts 三個標頭讓客戶把照片對回那筆 JSON。
/// 接收端不想收照片就不必實作,原本的 JSON 流程完全不受影響。
fn report_photo(
    nvs: &EspNvs<NvsDefault>,
    device: &str,
    task_id: u32,
    ts: u64,
    jpeg: &[u8],
) -> Result<()> {
    if !report_photo_enabled(nvs) {
        return Ok(());
    }
    let url = match nvs_get(nvs, REPORT_URL_KEY) {
        Some(u) if !u.trim().is_empty() => u,
        _ => return Ok(()),
    };
    let tid = task_id.to_string();
    let tss = ts.to_string();
    let token = nvs_get(nvs, REPORT_TOKEN_KEY).unwrap_or_default();
    let auth = format!("Bearer {}", token.trim());
    let mut headers: Vec<(&str, &str)> = vec![
        ("Content-Type", "image/jpeg"),
        ("X-Device", device),
        ("X-Task-Id", tid.as_str()),
        ("X-Ts", tss.as_str()),
    ];
    if !token.trim().is_empty() {
        headers.push(("Authorization", auth.as_str()));
    }
    info!("上報照片 {} bytes -> {}", jpeg.len(), url);
    http_post_bytes_ex(&url, &headers, jpeg, true)?;
    Ok(())
}

/// 從一段回覆裡抽出「數值」。上報要的是可入庫的欄位,不是一整段話。
/// 抓第一個數字(含小數),抓不到就回空字串 —— 空字串代表「這輪沒讀到」,
/// 比硬塞一個 0 進資料庫安全。
fn extract_value(text: &str) -> String {
    // 「1,520」原本在逗號處截斷成「1」，寫進客戶資料庫就是假的低點；負號也會被丟掉。
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut seen_digit = false;
    for (i, &ch) in chars.iter().enumerate() {
        let next_is_digit = chars.get(i + 1).map_or(false, |c| c.is_ascii_digit());
        if ch.is_ascii_digit() {
            out.push(ch);
            seen_digit = true;
        } else if ch == '-' && !seen_digit && out.is_empty() && next_is_digit {
            out.push(ch);
        } else if ch == '.' && seen_digit && !out.contains('.') {
            out.push(ch);
        } else if ch == ',' && seen_digit && next_is_digit {
            continue; // 千分位逗號
        } else if seen_digit {
            break;
        }
    }
    out.trim_end_matches('.').to_string()
}

/// 這一次視覺判讀該用多少 temperature。
///
/// 優先序:使用者指定 > 本輪重試調高後的值 > 預設。
/// 「重試就調高」是刻意的:判讀失敗多半是模型卡在某個固定輸出上,
/// 用一模一樣的參數重跑通常還是同一個結果,等於白花一次額度。
fn current_vision_temp() -> f32 {
    let fixed = AI_TEMP_FIXED_X100.load(Ordering::Relaxed);
    if fixed > 0 {
        return (fixed as f32) / 100.0;
    }
    let cur = VISION_TEMP_X100.load(Ordering::Relaxed);
    if cur > 0 {
        (cur as f32) / 100.0
    } else {
        VISION_TEMP_BASE
    }
}

/// 這一輪失敗了 —— 把下一次的 temperature 往上推一格(使用者有指定就不動)。
fn escalate_vision_temp() {
    if AI_TEMP_FIXED_X100.load(Ordering::Relaxed) > 0 {
        return; // 使用者自己指定就尊重他,不要偷偷改
    }
    let cur = current_vision_temp();
    let next = (cur + VISION_TEMP_STEP).min(VISION_TEMP_MAX);
    VISION_TEMP_X100.store((next * 100.0) as u32, Ordering::Relaxed);
    info!("判讀失敗,下次 temperature {:.1} -> {:.1}", cur, next);
}

/// 成功了就回到基準,不要讓一次失敗永遠影響後面每一輪。
fn reset_vision_temp() {
    VISION_TEMP_X100.store(0, Ordering::Relaxed);
}

fn capture_camera_jpeg(pins: &CameraPins) -> Result<Vec<u8>> {
    use esp_idf_sys::camera;

    let _guard = CAMERA_LOCK.lock().map_err(|_| anyhow!("camera lock poisoned"))?;

    // Keep-alive: init once (at the boot camera test, while the heap is still
    // unfragmented and a contiguous 16KB DMA block is available) and hold it.
    // The camera's internal-RAM DMA buffer can't be reliably re-acquired later
    // (heap fragments -> "DMA buffer 16384 malloc failed, largest free block
    // 6912"), and PSRAM DMA corrupts JPEGs on this OV3660 board. So we grab it
    // early and keep it; WiFi static buffers were trimmed (sdkconfig.defaults)
    // to leave enough internal RAM for TLS sockets to coexist with this hold.
    if !CAMERA_READY.load(Ordering::Relaxed) {
        let config = camera_init_config(pins);
        unsafe { esp_ok(camera::esp_camera_init(&config), "esp_camera_init")?; }
        CAMERA_READY.store(true, Ordering::Relaxed);
        apply_camera_orientation();
        info!("Camera initialized (keep-alive)");
    }

    unsafe {
        // 先丟掉緩衝區裡那張過期的畫面。
        //
        // 設定是 fb_count=1 + CAMERA_GRAB_WHEN_EMPTY,這個組合的時序是:
        //   fb_get()    拿到緩衝區裡的畫面
        //   fb_return() 緩衝區空了 -> 驅動「立刻」擷取下一張存進去
        //   ...時間過去,現場已經變了...
        //   fb_get()    拿到的是**上次 return 那一刻**拍的,不是現在的
        //
        // 所以每次拍照拿到的其實是上一次拍照結束當下的畫面 —— 使用者回報的
        // 「有時候拍照會給上一張」就是這個。定時任務間隔越長,畫面越舊,
        // 對監控來說等於是拿過期資料在告警。
        //
        // 丟一張再拿一張,拿到的才是「現在」。代價是一個影格的時間(數十毫秒)。
        if CAMERA_READY.load(Ordering::Relaxed) {
            let stale = camera::esp_camera_fb_get();
            if !stale.is_null() {
                camera::esp_camera_fb_return(stale);
            }
        }

        let fb = camera::esp_camera_fb_get();
        if fb.is_null() {
            // Reinit once on failure
            warn!("Camera fb null, reinitializing...");
            let _ = camera::esp_camera_deinit();
            CAMERA_READY.store(false, Ordering::Relaxed);
            let config = camera_init_config(pins);
            esp_ok(camera::esp_camera_init(&config), "esp_camera_reinit")?;
            CAMERA_READY.store(true, Ordering::Relaxed);
            let fb = camera::esp_camera_fb_get();
            if fb.is_null() {
                bail!("esp_camera_fb_get returned null after reinit");
            }
            let data = std::slice::from_raw_parts((*fb).buf, (*fb).len).to_vec();
            camera::esp_camera_fb_return(fb);
            return Ok(data);
        }
        let data = std::slice::from_raw_parts((*fb).buf, (*fb).len).to_vec();
        camera::esp_camera_fb_return(fb);
        Ok(data)
    }
}

/// 在「總時限」與「長度上限」內讀出 HTTP 請求首行;超時、超長、斷線、非 UTF-8 都回 None。
/// socket 的 read timeout 只管「單次」read —— 每兩秒滴一個位元組就能一直續命,
/// 所以每次 read 前都把 timeout 重設成「離總期限還剩多久」,設不成功就直接放棄。
///
/// ⚠️ ESP-IDF 的 lwIP 把 SO_RCVTIMEO 截成整數毫秒,而 0 毫秒代表「永久等待」——
/// 剩下不到 1ms 時照設會變成無限阻塞。所以剩不到 MIN_WAIT 就直接拒絕,不再 read。
#[cfg(esp_idf_comp_espressif__esp32_camera_enabled)]
fn read_request_line_bounded(stream: &mut std::net::TcpStream, total: Duration, max: usize) -> Option<String> {
    use std::io::Read;
    const MIN_WAIT: Duration = Duration::from_millis(10);
    let deadline = std::time::Instant::now().checked_add(total)?;
    let mut line: Vec<u8> = Vec::with_capacity(128);
    let mut buf = [0u8; 128];
    loop {
        let left = deadline.checked_duration_since(std::time::Instant::now())?;
        if left < MIN_WAIT {
            return None;
        }
        stream.set_read_timeout(Some(left)).ok()?;
        let n = stream.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        line.extend_from_slice(&buf[..n]);
        // 先量首行長度再看有沒有換行:超長的首行即使這一批剛好帶到換行也不收。
        let newline = line.iter().position(|&b| b == b'\n');
        if newline.unwrap_or(line.len()) > max {
            return None;
        }
        if let Some(pos) = newline {
            line.truncate(pos);
            return String::from_utf8(line).ok();
        }
    }
}

/// Start a background MJPEG HTTP stream thread on `port` with token auth.
/// Browsers/VLC can open: http://<device-ip>:<port>/stream?token=<random>
/// Call STREAM_ACTIVE.store(false) or /camera stream stop to shut down.
#[cfg(esp_idf_comp_espressif__esp32_camera_enabled)]
fn start_mjpeg_stream_thread(pins: CameraPins, port: u16, stream_token: String) {
    use std::io::Write;
    use std::net::TcpListener;

    STREAM_ACTIVE.store(true, Ordering::Relaxed);

    let _ = std::thread::Builder::new()
        .name("mjpeg".to_string())
        .stack_size(16 * 1024)
        .spawn(move || {
            let bind_addr = format!("0.0.0.0:{}", port);
            let listener = match TcpListener::bind(&bind_addr) {
                Ok(l) => l,
                Err(e) => {
                    warn!("MJPEG bind failed: {}", e);
                    STREAM_ACTIVE.store(false, Ordering::Relaxed);
                    return;
                }
            };
            // accept() will block; set a 3s read timeout so we can check STREAM_ACTIVE
            let _ = listener.set_nonblocking(false);
            info!("MJPEG stream listening on :{}", port);

            while STREAM_ACTIVE.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _addr)) => {
                        // 驗 token 之前就有上限:整個首行最多讀 3 秒(總時限,不是單次)、最多 512 bytes。
                        // 否則區網任何人連上來慢慢滴資料或不送換行(佔住唯一的串流執行緒)都不用 token。
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                        let expected = format!("token={}", stream_token);
                        let authorized = read_request_line_bounded(&mut stream, Duration::from_secs(3), 512)
                            .map_or(false, |line| line.contains(&expected));
                        if !authorized {
                            let _ = stream.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\nAccess denied. Invalid or missing token.\n");
                            continue;
                        }

                        // Send HTTP 200 + MJPEG multipart headers
                        let hdr = b"HTTP/1.1 200 OK\r\n\
                            Content-Type: multipart/x-mixed-replace; boundary=--jpgframe\r\n\
                            Cache-Control: no-cache, no-store, must-revalidate\r\n\
                            Pragma: no-cache\r\n\
                            Access-Control-Allow-Origin: *\r\n\
                            \r\n";
                        if stream.write_all(hdr).is_err() { continue; }

                        // Stream frames until told to stop or client disconnects
                        while STREAM_ACTIVE.load(Ordering::Relaxed) {
                            match capture_camera_jpeg(&pins) {
                                Ok(jpeg) => {
                                    let part_hdr = format!(
                                        "--jpgframe\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                                        jpeg.len()
                                    );
                                    if stream.write_all(part_hdr.as_bytes()).is_err() { break; }
                                    if stream.write_all(&jpeg).is_err() { break; }
                                    if stream.write_all(b"\r\n").is_err() { break; }
                                }
                                Err(e) => {
                                    warn!("MJPEG capture error: {}", e);
                                    std::thread::sleep(Duration::from_millis(500));
                                }
                            }
                            std::thread::sleep(Duration::from_millis(150)); // ~6 fps
                        }
                    }
                    Err(e) => {
                        // socket 用盡(EMFILE)等錯誤要看得到,否則外部連線只會被 RST
                        warn!("MJPEG accept error: {}", e);
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
            }
            info!("MJPEG stream thread stopped");
        });
}

fn send_resend_email(api_key: &str, from: &str, to: &str, subject: &str, body: &str) -> Result<String> {
    let payload = serde_json::json!({
        "from": from,
        "to": [to],
        "subject": subject,
        "text": body,
    });
    let json_body = serde_json::to_string(&payload)?;
    let body_bytes = json_body.as_bytes();
    let content_len = body_bytes.len().to_string();
    let auth = format!("Bearer {}", api_key);

    let mut client = EspHttpConnection::new(&new_http_config(60))?;
    client.initiate_request(
        esp_idf_svc::http::Method::Post,
        RESEND_API_URL,
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &content_len),
            ("Authorization", &auth),
        ],
    )?;
    client.write_all(body_bytes)?;
    client.initiate_response()?;

    let mut resp = Vec::with_capacity(2048);
    let mut buf = [0u8; 1024];
    loop {
        match client.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => resp.extend_from_slice(&buf[..n]),
            Err(e) => {
                warn!("email read: {:?}", e);
                break;
            }
        }
    }
    Ok(String::from_utf8_lossy(&resp).to_string())
}

fn handle_direct_email(
    cfg: &Config,
    nvs: &mut EspNvs<NvsDefault>,
    chat_id: i64,
    arg: &str,
) {
    if let Some(rest) = arg.strip_prefix("config ") {
        let parts: Vec<&str> = rest.splitn(3, '|').collect();
        if parts.len() < 3 {
            let _ = send_telegram(&cfg.tg_token, chat_id,
                "Usage: /email config from@example.com|default_to@example.com|resend_api_key");
            return;
        }
        let _ = nvs.set_str("email_from", parts[0].trim());
        let _ = nvs.set_str("email_to", parts[1].trim());
        let _ = nvs.set_str("email_api_key", parts[2].trim());
        let _ = send_telegram(&cfg.tg_token, chat_id,
            &format!("✅ 已儲存 ESP 郵件設定\n寄件者：{}\n預設收件者：{}", parts[0].trim(), parts[1].trim()));
        return;
    }

    if arg == "status" || arg.is_empty() {
        let from = nvs_get(nvs, "email_from").unwrap_or_else(|| "(未設定)".to_string());
        let to = nvs_get(nvs, "email_to").unwrap_or_else(|| "(未設定)".to_string());
        let key = if nvs_get(nvs, "email_api_key").is_some() { "已設定" } else { "未設定" };
        let _ = send_telegram(&cfg.tg_token, chat_id,
            &format!("ESP 郵件功能狀態\n寄件者：{}\n預設收件者：{}\nAPI Key：{}\n\n用法：\n/email config from|to|resend_key\n/email to@example.com|主旨|內容\n/email |主旨|內容  (使用預設收件者)", from, to, key));
        return;
    }

    let parts: Vec<&str> = arg.splitn(3, '|').collect();
    if parts.len() < 2 {
        let _ = send_telegram(&cfg.tg_token, chat_id,
            "Usage: /email to@example.com|subject|body\n💡 /email |subject|body 使用預設收件者");
        return;
    }

    let api_key = match nvs_get(nvs, "email_api_key") {
        Some(v) => v,
        None => {
            let _ = send_telegram(&cfg.tg_token, chat_id,
                "ESP 郵件功能尚未設定，請先執行 /email config from|to|resend_api_key");
            return;
        }
    };
    let from = match nvs_get(nvs, "email_from") {
        Some(v) => v,
        None => {
            let _ = send_telegram(&cfg.tg_token, chat_id,
                "缺少寄件人地址，請先 /email config 設定");
            return;
        }
    };

    let default_to = nvs_get(nvs, "email_to").unwrap_or_default();
    let to = if parts.len() == 2 || parts[0].trim().is_empty() {
        default_to.as_str()
    } else {
        parts[0].trim()
    };
    let subject = if parts.len() == 2 { parts[0].trim() } else { parts[1].trim() };
    let body = if parts.len() == 2 { parts[1].trim() } else { parts[2].trim() };

    if to.is_empty() {
        let _ = send_telegram(&cfg.tg_token, chat_id, "沒有收件者，請設定 to 或預先設置預設收件者");
        return;
    }

    let _ = send_telegram(&cfg.tg_token, chat_id, &format!("📧 正在透過 ESP 寄信到 {}...", to));
    match send_resend_email(&api_key, &from, to, subject, body) {
        Ok(resp) => {
            let preview = str_prefix(&resp, 200);
            let _ = send_telegram(&cfg.tg_token, chat_id,
                &format!("✅ 郵件已送出 → {}\n主旨: {}\n預覽: {}", to, subject, preview));
        }
        Err(e) => {
            let _ = send_telegram(&cfg.tg_token, chat_id,
                &format!("❌ ESP 郵件寄送失敗: {}\n如果設定正確，可能是本裝置不支援直接寄信，試試 PC Driver SMTP 方式", e));
        }
    }
}

// ===== Diagnostic System =====

struct DiagEntry {
    timestamp: u64,
    level: &'static str,
    message: String,
}

struct DiagBuffer {
    entries: Vec<DiagEntry>,
}

impl DiagBuffer {
    fn new() -> Self {
        Self { entries: Vec::new() }
    }

    fn log(&mut self, level: &'static str, msg: &str) {
        if self.entries.len() >= MAX_DIAG_ENTRIES {
            self.entries.remove(0);
        }
        self.entries.push(DiagEntry {
            timestamp: now_epoch(),
            level,
            message: msg.to_string(),
        });
    }

    fn format_report(&self) -> String {
        if self.entries.is_empty() {
            return "No diagnostic entries.".to_string();
        }
        let mut s = format!("Diagnostics ({} entries):\n", self.entries.len());
        for e in &self.entries {
            let (h, m) = epoch_to_hhmm(e.timestamp);
            s.push_str(&format!("[{:02}:{:02}] {} {}\n", h, m, e.level, e.message));
        }
        s
    }

    fn format_for_gemini(&self) -> String {
        if self.entries.is_empty() {
            return String::new();
        }
        let mut s = "\n\nRecent system diagnostics:\n".to_string();
        for e in self.entries.iter().rev().take(10) {
            s.push_str(&format!("- [{}] {}\n", e.level, e.message));
        }
        s
    }
}

// ===== SKILL System =====

#[derive(Clone)]
struct Skill {
    name: String,
    description: String,
    trigger: String,
}

fn load_skills_from_nvs(nvs: &EspNvs<NvsDefault>) -> Vec<Skill> {
    let count: usize = nvs_get(nvs, "sk_cnt")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut skills = Vec::new();
    for i in 0..count.min(MAX_SKILLS) {
        let nkey = format!("sk{:02}", i);
        if let Some(val) = nvs_get(nvs, &nkey) {
            let parts: Vec<&str> = val.splitn(3, '|').collect();
            if parts.len() >= 3 {
                skills.push(Skill {
                    name: parts[0].to_string(),
                    description: parts[1].to_string(),
                    trigger: parts[2].to_string(),
                });
            }
        }
    }
    skills
}

fn save_skills_to_nvs(nvs: &mut EspNvs<NvsDefault>, skills: &[Skill]) {
    // 先刪尾巴、再寫、內容沒變不寫。NVS 是 log-structured：刪除只翻位元圖不佔空間，
    // 寫入才需要新條目。原本先寫後刪，在 24KB 幾乎滿的 NVS 上每筆 set_str 都
    // NOT_ENOUGH_SPACE 靜默失敗、尾巴卻真的被刪 → 技能清單壞掉（2026-09-09 實機驗證 NVS 已滿）。
    // 從尾巴刪技能因此變成零寫入、純釋放，是清 NVS 空間最安全的方法。
    for i in skills.len()..MAX_SKILLS {
        let _ = nvs.remove(&format!("sk{:02}", i));
    }
    let cnt = skills.len().to_string();
    if nvs_get(nvs, "sk_cnt").as_deref() != Some(cnt.as_str()) {
        let _ = nvs.set_str("sk_cnt", &cnt);
    }
    for (i, sk) in skills.iter().enumerate() {
        let nkey = format!("sk{:02}", i);
        let combined = format!("{}|{}|{}", sk.name, sk.description, sk.trigger);
        if nvs_get(nvs, &nkey).as_deref() == Some(combined.as_str()) {
            continue;
        }
        if nvs.set_str(&nkey, &combined).is_err() {
            warn!("技能 {} 寫入 NVS 失敗（空間不足？）", nkey);
        }
    }
}

fn format_skills_for_prompt(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "\n\n== ACTIVE SKILLS ??MANDATORY RULES ==\n\
        When user input matches a skill's TRIGGER pattern, you MUST emit the exact tag/syntax described.\n\
        Do NOT describe the command in prose ??execute directly by including the tag in your reply.\n\
        Emitting the correct tag IS the action; the firmware handles the rest.\n"
    );
    for sk in skills {
        s.push_str(&format!("\n[SKILL: {}]\n  TRIGGER: {}\n  ACTION (follow exactly): {}\n",
            sk.name, sk.trigger, sk.description));
    }
    s.push_str("\n== END SKILLS ==\n");
    s
}

/// 技能的分類與中文說明,只給控制台的技能瀏覽器用(AI 吃的是英文 description)。
///
/// 分類不存進 NVS —— 它是內建技能的固定屬性,存了只會多一次格式遷移。
/// 認不得的名字(使用者自訂技能)歸「其他」。
fn skill_meta(name: &str) -> (&'static str, &'static str) {
    match name {
        "camera_snap" => ("眼睛", "叫她拍一張照片傳給你"),
        "camera_vision" => ("眼睛", "拍照並告訴你畫面裡有什麼"),
        "watch_monitor" => ("眼睛", "每隔幾分鐘自己檢查一次，只有真的發生才通知你"),
        "auto_workflow" => ("排程", "每隔幾分鐘固定拍照回報一次（不管有沒有事）"),
        "task_control" => ("排程", "查看、停掉正在跑的定期任務"),
        "reminder_control" => ("排程", "每天／每隔多久提醒你一件事"),
        "board_voice_control" => ("裝置", "讓她安靜、只講重點、或講完整內容"),
        "board_wake_control" => ("裝置", "開關語音喚醒、改喚醒詞"),
        "board_wifi_help" => ("裝置", "查看與切換 WiFi"),
        "lcd_expression" => ("裝置", "螢幕上的表情跟著回覆心情換"),
        "pc_control_bridge" => ("電腦", "操作你的電腦：開檔案、跑指令、寫檔"),
        "desktop_screenshot" => ("電腦", "把電腦畫面截圖傳給你"),
        "code_gen" => ("電腦", "寫 Python 程式並在電腦上跑"),
        "excel" => ("電腦", "讀取、分析、修改 Excel 檔"),
        "calculator" => ("助理", "算數學"),
        "translator" => ("助理", "翻譯"),
        "image_gen" => ("助理", "畫圖"),
        _ => ("其他", ""),
    }
}

fn builtin_skills() -> Vec<Skill> {
    vec![
        Skill {
            name: "calculator".to_string(),
            description: "Perform math calculations".to_string(),
            trigger: "calculate".to_string(),
        },
        Skill {
            name: "translator".to_string(),
            description: "Translate text between languages".to_string(),
            trigger: "translate".to_string(),
        },
        Skill {
            name: "code_gen".to_string(),
            description: "Generate Python code and execute on PC using [PY:...] or [PYBLOCK]...[/PYBLOCK]".to_string(),
            trigger: "code".to_string(),
        },
        Skill {
            name: "image_gen".to_string(),
            description: "Generate images from text description with [IMG:prompt]".to_string(),
            trigger: "draw".to_string(),
        },
        Skill {
            name: "excel".to_string(),
            description: "Read, analyze, modify Excel spreadsheets with [EXCEL:path>>>instruction]".to_string(),
            trigger: "excel".to_string(),
        },
        Skill {
            name: "board_voice_control".to_string(),
            description: "Controls board TTS speech. ALWAYS emit exactly one tag AND confirm the new mode in text.
  [CTRL:voice:off]    \u{2705} board goes completely silent (no TTS for ANY message, wake word, or AutoTask)
  [CTRL:voice:normal] \u{2705} board speaks full replies
  [CTRL:voice:brief]  \u{2705} board speaks first sentence only
  TRIGGERS: mute/unmute/silence/quiet/voice on/voice off/brief/tts
  IMPORTANT: After emitting the tag, state the mode change clearly so user knows it took effect.".to_string(),
            trigger: "voice|mute|unmute|tts|brief|silent".to_string(),
        },
        Skill {
            name: "board_wake_control".to_string(),
            description: "When user asks to enable wake, disable wake, or change the wake phrase, emit exactly one control tag: [CTRL:wake:on], [CTRL:wake:off], or [CTRL:wake:set:phrase]. Use the full phrase inside the tag and avoid prose-only answers.".to_string(),
            trigger: "wake".to_string(),
        },
        Skill {
            name: "board_wifi_help".to_string(),
            description: "Explain runtime Wi-Fi commands: /wifi status, /wifi set<N> SSID PASSWORD (N=1-5), /wifi del <N>, /wifi swap, /wifi clear".to_string(),
            trigger: "wifi".to_string(),
        },
        Skill {
            name: "reminder_control".to_string(),
            description: "Manages Reminders. Rules:\
  Rule 1 - List: emit [REMIND:list]; board auto-replies with full list (ID + countdown).\
  Rule 2 - Delete: emit [REMIND:del:ID] (get ID from the list).\
  Rule 3 - If user wants to stop/delete but gave no ID, emit [REMIND:list] first to show IDs, then emit [REMIND:del:ID].\
  Syntax: [REMIND:interval:MINUTES:msg] / [REMIND:daily:HH:MM:msg] / [REMIND:once:HH:MM:msg].\
  Shortcut cmds: /reminders (view) | /remind del ID (delete one) | /reminders clear (delete all).\
  NEVER use prose only -- always emit the correct tag AND confirm.".to_string(),
            trigger: "remind|reminder|alarm|reminders".to_string(),
        },
        Skill {
            name: "pc_control_bridge".to_string(),
            description: "When user asks for desktop actions, emit an exact PC tag instead of describing what to type. Use [PC:screenshot], [PC:status], [PC:shell:cmd], [PC:file_read:path], [PC:file_list:path], or [PC:file_write:path>>>content]. Keep the tag exact and place it before any short confirmation text.".to_string(),
            trigger: "pc".to_string(),
        },
        Skill {
            name: "desktop_screenshot".to_string(),
            description: "For screenshot requests, emit exactly [PC:screenshot]. Do not explain the command or mention alternate syntax unless asked. The desktop driver will return the image to Telegram.".to_string(),
            trigger: "screenshot".to_string(),
        },
        Skill {
            name: "lcd_expression".to_string(),
            description: "Put exactly ONE face tag at the start of every reply. The board has 4 illustrated portraits:\
  [FACE:neutral] calm  |  [FACE:smile] gentle smile  |  [FACE:happy] big laugh  |  [FACE:love] hands making a heart\
  Target mix: neutral 30% / smile 30% / happy 30% / love 10%. Do NOT repeat the same face many times in a row.\
  [FACE:love] is the rare one - save it for real affection or thanks, never as a default.".to_string(),
            trigger: "face".to_string(),
        },
        Skill {
            name: "watch_monitor".to_string(),
            description: "Recurring CONDITIONAL camera monitoring - the board checks something on a timer and \
only speaks up when the condition is actually met (stays completely silent otherwise).\
  Syntax: [AUTOTASK:watch:INTERVAL_MIN:condition:speak]  (speak=also say it out loud, nospeak=Telegram only)\
  Write the condition in Traditional Chinese, as one plain question about what to look for.\
  Example: user says '\u{6bcf}5\u{5206}\u{9418}\u{770b}\u{6559}\u{5ba4}\u{6709}\u{6c92}\u{6709}\u{4eba}\u{7761}\u{8457},\u{6709}\u{5c31}\u{544a}\u{8a34}\u{6211},\u{6c92}\u{6709}\u{5c31}\u{4e0d}\u{8981}\u{5410}\u{69fd}'\
    -> [AUTOTASK:watch:5:\u{6559}\u{5ba4}\u{88e1}\u{662f}\u{5426}\u{6709}\u{4eba}\u{8DA4}\u{5728}\u{684c}\u{4e0a}\u{6216}\u{9589}\u{773c}\u{7761}\u{8457}:speak]\
  Example: '\u{6bcf}30\u{5206}\u{9418}\u{770b}\u{4e00}\u{4e0b}\u{770b}\u{677f}\u{4e0a}\u{7684}\u{7522}\u{91cf}\u{6578}\u{5b57}' -> [AUTOTASK:watch:30:\u{8b80}\u{51fa}\u{756b}\u{9762}\u{4e2d}\u{770b}\u{677f}\u{4e0a}\u{7684}\u{7522}\u{91cf}\u{6578}\u{5b57}:nospeak]\
  IMPORTANT: use 'watch' (not 'camera') whenever the user says '\u{6709}...\u{624d}...' / '\u{6c92}\u{6709}\u{5c31}\u{4e0d}\u{7528}\u{8b1b}' / \
'\u{4e0d}\u{8981}\u{4e00}\u{76f4}\u{5410}\u{69fd}' - i.e. any time they only want to hear about exceptions. \
Use 'camera' only when they want a report EVERY time regardless of what is seen.\
  The board wraps the condition into a strict JSON prompt itself - do not write JSON in the tag.".to_string(),
            trigger: "watch|monitor|\u{76e3}\u{770b}|\u{76e3}\u{63a7}|\u{5de1}\u{67e5}|\u{6709}\u{4eba}\u{5c31}|\u{767c}\u{751f}\u{5c31}".to_string(),
        },
        Skill {
            name: "camera_snap".to_string(),
            description: "ONLY when user asks to take a photo WITHOUT any analysis request (just \u{62cd}\u{7167}/snap/photo with nothing else). Emit [CAM:snap]. If user also asks a question about the photo, use camera_vision instead!".to_string(),
            trigger: "photo".to_string(),
        },
        Skill {
            name: "camera_vision".to_string(),
            description: "When user wants photo+analysis or asks to look/identify (\u{62cd}\u{7167}\u{8acb}\u{544a}\u{8a34}\u{6211}.../\u{770b}\u{4e00}\u{4e0b}/\u{5e6b}\u{6211}\u{770b}/\u{8fa8}\u{8b58}/\u{5206}\u{6790}): emit [CAM:vision:user's question or description request]. Always prefer this over snap when user wants ANY information about what's in the photo.".to_string(),
            trigger: "vision".to_string(),
        },
        Skill {
            name: "auto_workflow".to_string(),
            description: "Sets up a recurring background AutoTask (camera + AI analysis + optional speech).\
  Syntax: [AUTOTASK:camera:INTERVAL_MIN:AI_prompt:speak] where speak=TTS, nospeak=send Telegram.\
  Delete a task: [AUTOTASK:del:ID]. List all tasks: [AUTOTASK:list].\
  Write the AI prompt in Traditional Chinese describing what Gemini should report.\
  Example: user says every 5 min take photo and say who is sleeping -> [AUTOTASK:camera:5:請描述畫面中誰在睡覺:speak].\
  Tip: interval < 5 min causes heavy API load; recommend 5+ min.".to_string(),
            trigger: "autotask|workflow|auto task|recurring".to_string(),
        },
        Skill {
            name: "task_control".to_string(),
            description: "Manages board background AutoTasks.\
  Rule 1 - List tasks: emit [AUTOTASK:list]; board replies with task list + IDs.\
  Rule 2 - Stop/cancel/delete: emit [AUTOTASK:del:ID] (get ID from list first if unknown).\
  Rule 3 - If user says stop/kill without giving ID, emit [AUTOTASK:list] to get IDs, then emit [AUTOTASK:del:ID].\
  Rule 4 - Clear all: user can type /tasks clear.\
  Shortcut cmds (faster): /tasks (list), /tasks del ID (stop one), /tasks clear (all).\
  NEVER describe the action in prose only -- always emit the correct tag.".to_string(),
            trigger: "task|tasks|kill task|stop task|autotask list|autotask del".to_string(),
        },
        // ── 工業監控情境 ──────────────────────────────────────────────
        // 這幾條的存在理由:使用者不會講「建立 autotask watch」,他會講
        // 「幫我盯著產量」「有人沒戴安全帽就叫我」。沒有對應的 skill,模型只會
        // 用聊天方式回答,不會真的去建任務。description 會進 system prompt,
        // 等於直接教模型該發哪個標籤。
        Skill {
            name: "gauge_reading".to_string(),
            description: "Read numbers off a dashboard, gauge, meter or display board and report them.\
  For a ONE-OFF reading right now: reply with the number only, no scene description.\
  For RECURRING readings the user wants reported every time (production counts, temperature\
  logs, meter readings), emit [AUTOTASK:camera:INTERVAL_MIN:讀出畫面中的<什麼>數字,只回數字:nospeak].\
  Use camera (reports every time) NOT watch (only reports when a condition is met) --\
  the user wants the value logged each round, not an alert.\
  If the number is not visible or too blurry to read, say so. NEVER guess a number:\
  a wrong gauge reading is worse than no reading.\
  If the user reports wrong or missing readings, the usual cause is that the LABEL next to\
  the number is too small for the camera to resolve -- the digits are legible but the label\
  is not, so the value cannot be matched to the right item. Tell them to either move the\
  camera closer, or ask using wording that identifies the value by position instead\
  (e.g. 最上面那個數字 / 左邊第二個).".to_string(),
            trigger: "產量|讀數|儀表|錶|看板|數字是多少|溫度幾度|壓力多少|gauge|meter|dashboard reading".to_string(),
        },
        Skill {
            name: "safety_watch".to_string(),
            description: "Workplace safety monitoring via the camera.\
  Typical asks: people not wearing helmets/hard hats, someone in a danger zone, someone\
  collapsed or asleep, machine left running unattended.\
  Emit [AUTOTASK:watch:INTERVAL_MIN:<condition in plain Chinese>:speak] -- watch only\
  notifies when the condition IS met, staying silent otherwise, which is what safety\
  monitoring needs. Add speak so it also sounds an alarm on the device.\
  When counting people, give the count as a plain number and say so if unsure.".to_string(),
            trigger: "安全帽|沒戴|工地|危險區|有人跌倒|有人昏倒|有人睡著|helmet|hard hat|safety".to_string(),
        },
        Skill {
            name: "region_watch".to_string(),
            description: "Watch only ONE region of the frame instead of the whole picture.\
  The user sets the box by dragging on the live view in the web console; it is stored as a\
  ROI prefix on the task and applied automatically -- you do not need to compute coordinates.\
  If the user says things like 'only watch that corner' / '只看左邊那台機器', tell them to\
  drag a box on the 排程 tab of the console first, then create the watch task as usual.".to_string(),
            trigger: "只看|框選|只盯|某一區|那一塊|region|roi|crop".to_string(),
        },
        Skill {
            name: "monitor_advisor".to_string(),
            description: "Universal handler for ANY monitoring request. Before creating a task you need\
  three things: WHAT to look for, HOW OFTEN, and WHETHER it should also make a sound.\
  If the user gave you all three, just create the task.\
  If something is missing, ASK -- do not silently invent an interval. Guessing '5 minutes'\
  when they meant 'every hour' either burns their AI quota or misses the event.\
  Suggest a sensible default when you ask, e.g. 'safety checks usually 3-5 min,\
  meter logging usually 15-60 min -- which do you want?'\
  Also apply these judgements without being asked:\
  - Anything the user must ACT on quickly (fall, fire, intruder, danger zone) -> watch + speak.\
  - Anything they just want a record of (counts, meter values) -> camera, no speak.\
  - Interval under 1 minute: warn that it burns quota fast and offer 2-3 min instead.\
  - If what they describe cannot be seen in a photo at all (sound, smell, weight), say so\
    plainly instead of creating a task that can never fire.\
  - Physical quantities -- temperature, humidity, pressure, speed -- are readable ONLY if a\
    display or gauge showing them is in the camera's view. Do NOT assume one is there.\
    ASK first: 'is there a thermometer/gauge in view? if yes I will read it every N minutes,\
    if not I cannot see temperature.' Creating the task blindly means it reports\
    'cannot see it' every single round, forever, while burning quota.".to_string(),
            trigger: "幫我看|幫我盯|監控|監看|盯著|注意一下|watch for|keep an eye".to_string(),
        },
        Skill {
            name: "api_report_setup".to_string(),
            description: "Sets up pushing readings to the user's own API/database/dashboard.\
  Command: /report set <url>, /report token <token>, /report test, /report off.\
  The device validates the URL on save and rejects obviously wrong ones, but YOU should\
  catch problems first and ASK rather than passing junk through:\
  - No http:// or https:// prefix -> ask for the full URL.\
  - Only a domain with no path -> ask which endpoint path receives the data.\
  - They pasted a dashboard/login page URL rather than an API endpoint -> point that out.\
  - They mention auth/key/token but did not give one -> ask for it before setting the URL.\
  After setting, ALWAYS tell them to run /report test and check their server actually\
  received it -- a URL that saves fine can still 404.\
  The JSON body has fixed fields: device, task_id, task_type, ts, prompt, value, text, hit, ok.\
  Two things to tell them when they ask how to write the receiver:\
  watch tasks also report when nothing happened (hit=false) so they can tell 'all normal'\
  apart from 'not running'; and value is an empty string when nothing was readable,\
  never 0, because a fake 0 becomes a fake low point on their chart.".to_string(),
            trigger: "api|上報|回傳|傳到|資料庫|dashboard|webhook|endpoint|接收".to_string(),
        },
        Skill {
            name: "elder_care".to_string(),
            description: "Monitoring an elderly person living alone. The cost of errors is asymmetric here:\
  a false alarm is an annoyance, a missed fall can be fatal -- so lean towards reporting\
  when uncertain, and say in the watch condition that uncertain cases should count as a hit.\
  Common asks and how to build them:\
  - fell over / on the floor / not getting up -> watch, 5-10 min, speak on.\
  - has not moved for a long time -> watch with a condition mentioning the same posture\
    over consecutive checks; explain it compares single snapshots, not video, so it is\
    a rough signal.\
  - took their medicine / ate -> camera at meal times is more reliable than watch.\
  Also raise privacy once, briefly: the camera sends photos to Google for analysis, so\
  suggest pointing it at a living area rather than a bedroom or bathroom.".to_string(),
            trigger: "長輩|老人|奶奶|爺爺|媽媽|爸爸|跌倒|獨居|照護|elderly|fall".to_string(),
        },
        Skill {
            name: "troubleshoot".to_string(),
            description: "When the user says it is not working / no notification / no response,\
  diagnose in this order instead of telling them to reboot:\
  1. Emit [AUTOTASK:list] -- if there are no tasks, that is the answer.\
  2. Ask what the camera can actually see; suggest 'take a photo' and have them LOOK at it.\
     Most reported failures are the camera pointing at the wrong thing, a blocked lens,\
     or labels too small to resolve.\
  3. If readings come back wrong or empty, the usual cause is unreadable labels next to\
     the numbers -- tell them to move closer or ask by position instead.\
  4. If they say nothing at all arrives: a watch task is silent by design when the\
     condition is not met. Ask whether they expected a report every round (camera) or\
     only on events (watch).\
  5. Only after all of the above, consider WiFi/quota. /status shows connectivity.\
  Never reply with just 'try rebooting' -- it hides the real cause and it will come back.".to_string(),
            trigger: "沒反應|沒收到|不能用|壞了|怎麼沒|沒通知|失敗|not working|no notification".to_string(),
        },
    ]
}

/// 使用者用 /skill del 刪過的內建技能名單（NVS key `sk_deleted`，逗號分隔）。
/// 沒有這個，開機補種會把刪掉的內建技能全部長回來，24 個技能又把 24KB NVS 填滿。
fn deleted_builtins(nvs: &EspNvs<NvsDefault>) -> Vec<String> {
    nvs_get(nvs, "sk_deleted").unwrap_or_default()
        .split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}

fn set_deleted_builtins(nvs: &mut EspNvs<NvsDefault>, names: &[String]) {
    if names.is_empty() {
        let _ = nvs.remove("sk_deleted");
    } else {
        let _ = nvs.set_str("sk_deleted", &names.join(","));
    }
}

fn ensure_builtin_skills(skills: &mut Vec<Skill>, skip: &[String]) -> bool {
    let mut changed = false;
    for skill in builtin_skills() {
        if skip.iter().any(|d| d.eq_ignore_ascii_case(&skill.name)) {
            continue; // 使用者刻意刪掉的，不要長回來
        }
        if let Some(existing) = skills.iter_mut().find(|e| e.name.eq_ignore_ascii_case(&skill.name)) {
            // Update description/trigger if changed
            if existing.description != skill.description || existing.trigger != skill.trigger {
                existing.description = skill.description;
                existing.trigger = skill.trigger;
                changed = true;
            }
            continue;
        }
        if skills.len() >= MAX_SKILLS {
            break;
        }
        skills.push(skill);
        changed = true;
    }
    changed
}

// ===== USB PC Driver Protocol =====

static USB_CMD_ID: AtomicU32 = AtomicU32::new(0);
/// Mutex to prevent multi-thread interleaving of JSON lines on UART0 TX.
static SERIAL_TX_LOCK: Mutex<()> = Mutex::new(());
/// Set to true once PC driver responds (handshake / any valid JSON ack).
static DRIVER_CONNECTED: AtomicBool = AtomicBool::new(false);

/// PC Safe Mode — when true (default), AI-generated [PC:], [PY:], [IMG:], [EXCEL:] tags
/// are blocked. Only explicit user commands (e.g. /screenshot) and safe read-only queries
/// are allowed. User can toggle via /pc_unlock and /pc_lock.
static PC_SAFE_MODE: AtomicBool = AtomicBool::new(true);
/// /pc_unlock 的到期時間(uptime 秒)。解鎖不能是永久的:忘了鎖回去,之後任何一張
/// 被動過手腳的圖片或文字都可能誘導 AI 輸出 [PC:shell:...]。
static PC_UNLOCK_UNTIL: AtomicU32 = AtomicU32::new(0);
const PC_UNLOCK_SECS: u32 = 600;

/// Commands that are always safe even in safe mode (read-only or status queries).
const PC_SAFE_CMDS: &[&str] = &["status", "screenshot", "clipboard_get", "process_list", "file_read", "file_list", "scan_inbox", "find_window"];

/// Check if a PC command is allowed under current safe mode.
fn pc_cmd_allowed(cmd: &str, from_ai: bool) -> bool {
    if !from_ai { return true; } // Direct user commands always pass
    if !PC_SAFE_MODE.load(Ordering::Relaxed) {
        if uptime_secs() < PC_UNLOCK_UNTIL.load(Ordering::Relaxed) {
            return true; // Safe mode off, still inside the unlock window
        }
        PC_SAFE_MODE.store(true, Ordering::Relaxed);
        info!("PC unlock 已到期,自動重新鎖定");
    }
    PC_SAFE_CMDS.contains(&cmd) // Only allow safe read-only commands
}

/// 麥克風轉寫出來的文字一律當「聊天內容」,不能變成斜線管理指令
/// (例如被轉寫成 /pc_unlock 或 / /pc shell ...)。開頭的斜線與空白交錯也要一起剝掉,
/// 剝完的結果保證不以斜線或空白開頭,handle_text 再 trim 也變不回指令。
fn voice_as_chat(transcript: &str) -> String {
    transcript
        .trim_start_matches(|c: char| c == '/' || c == '／' || c.is_whitespace())
        .to_string()
}

fn usb_next_id() -> String {
    let id = USB_CMD_ID.fetch_add(1, Ordering::Relaxed);
    format!("{:04}", id)
}

/// Send a JSON command to the PC driver via USB Serial (println ??UART0 ??CH340 ??PC).
/// PC driver reads JSON lines, executes, and sends result to user via Telegram.
fn usb_send_pc_cmd(cmd: &str, args: &serde_json::Value, chat_id: i64) {
    let id = usb_next_id();
    let msg = serde_json::json!({
        "cmd": cmd,
        "args": args,
        "id": id,
        "chat_id": chat_id.to_string(),
    });
    // Lock to prevent other threads' println!/info! from interleaving mid-JSON
    {
        let _lock = SERIAL_TX_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        println!("{}", msg);
    }
    info!("USB CMD #{}: {} -> PC", id, cmd);
}

fn spawn_serial_command_reader() -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("serial".into())
        .stack_size(4096)
        .spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = BufReader::new(stdin.lock());
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => std::thread::sleep(Duration::from_millis(100)),
                Ok(_) => {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        let _ = tx.send(trimmed.to_string());
                    }
                }
                Err(_) => std::thread::sleep(Duration::from_millis(250)),
            }
        }
    }).expect("serial thread spawn");
    rx
}

fn update_lcd_camera_status(nvs: &EspNvs<NvsDefault>, headline: &str, subtitle: &str) {
    let lcd_pins = load_lcd_pins(nvs);
    if let Err(err) = draw_lcd_scene(&lcd_pins, headline, subtitle) {
        warn!("LCD camera status update failed: {}", err);
    }
}

#[cfg(esp_idf_comp_espressif__esp32_camera_enabled)]
fn handle_camera_command(
    cfg: &Config,
    nvs: &mut EspNvs<NvsDefault>,
    state: &mut AppState,
    chat_id: i64,
    args: &str,
) {
    let trimmed = args.trim();

    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("status") {
        let pins = load_camera_pins(nvs);
        update_lcd_camera_status(nvs, "CAMERA", "Ready for snap / vision");
        let msg = format!(
            "OV3660 Camera\nCurrent model: {}\n\n{}\n\nCommands:\n/camera default\n/camera pins pwdn=-1 reset=-1 xclk=15 sda=4 scl=5 d0=11 d1=9 d2=8 d3=10 d4=12 d5=18 d6=17 d7=16 vsync=6 href=7 pclk=13\n/camera snap\n/camera vision\n/camera vision 說明桌上的東西",
            state.current_model,
            pins.status_text(),
        );
        let _ = send_telegram(&cfg.tg_token, chat_id, &msg);
        return;
    }

    // 相機解析度。讀「畫面上的小字」時這是成敗關鍵:拍 1920 寬的螢幕,
    // VGA(640) 等於縮到 1/3,原本 16px 的字只剩 5px,誰都讀不出來。
    // 改解析度要重新 init 相機才生效,所以設定完直接重開機。
    if let Some(rest) = trimmed.strip_prefix("res") {
        let arg = rest.trim().to_ascii_lowercase();
        let names = ["VGA 640x480", "SVGA 800x600", "XGA 1024x768",
                     "SXGA 1280x1024", "UXGA 1600x1200"];
        if arg.is_empty() {
            let cur = CAMERA_RES_MODE.load(Ordering::Relaxed).min(4) as usize;
            let _ = send_telegram(&cfg.tg_token, chat_id, &format!(
                "\u{1f4f7} 相機解析度：{}\n\n\
                 /camera res vga   640x480   最省\n\
                 /camera res svga  800x600\n\
                 /camera res xga   1024x768\n\
                 /camera res sxga  1280x1024\n\
                 /camera res uxga  1600x1200 預設,讀小字用這個\n\n\
                 讀不到畫面上的小字時就往上調。設定後會自動重開機生效。",
                names[cur]));
            return;
        }
        let m = match arg.as_str() {
            "vga" | "0" => Some(0u32),
            "svga" | "1" => Some(1),
            "xga" | "2" => Some(2),
            "sxga" | "3" => Some(3),
            "uxga" | "4" => Some(4),
            _ => None,
        };
        match m {
            Some(m) => {
                let _ = nvs.set_str(CAMERA_RES_KEY, &m.to_string());
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!(
                    "\u{2705} 相機解析度已設為 {}\n重開機套用中…", names[m as usize]));
                std::thread::sleep(Duration::from_millis(800));
                unsafe { esp_idf_svc::sys::esp_restart(); }
            }
            None => {
                let _ = send_telegram(&cfg.tg_token, chat_id,
                    "請用：vga / svga / xga / sxga / uxga");
            }
        }
        return;
    }

    // 相機擺放方向。裝置掛牆／倒吊／側放時畫面方向會不同，而讀儀表數字時
    // 顛倒的畫面會讓 AI 判讀變差，所以要能調。
    if let Some(rest) = trimmed.strip_prefix("flip") {
        let arg = rest.trim();
        if arg.is_empty() {
            let cur = CAMERA_FLIP_MODE.load(Ordering::Relaxed);
            let name = match cur { 1 => "上下翻轉", 2 => "左右鏡像", 3 => "旋轉180度", _ => "正常" };
            let _ = send_telegram(&cfg.tg_token, chat_id, &format!(
                "\u{1f4f7} 相機方向：{}（{}）\n\n\
                 /camera flip 0  正常\n\
                 /camera flip 1  上下翻轉\n\
                 /camera flip 2  左右鏡像\n\
                 /camera flip 3  旋轉 180 度（裝置倒著裝就用這個）", name, cur));
            return;
        }
        match arg.parse::<u32>() {
            Ok(m) if m <= 3 => {
                let _ = nvs.set_str(CAMERA_FLIP_KEY, &m.to_string());
                CAMERA_FLIP_MODE.store(m, Ordering::Relaxed);
                apply_camera_orientation();
                let name = match m { 1 => "上下翻轉", 2 => "左右鏡像", 3 => "旋轉180度", _ => "正常" };
                let _ = send_telegram(&cfg.tg_token, chat_id,
                    &format!("\u{2705} 相機方向已設為：{}（立即生效）", name));
            }
            _ => {
                let _ = send_telegram(&cfg.tg_token, chat_id, "請用 0~3：/camera flip 3");
            }
        }
        return;
    }

    if trimmed.eq_ignore_ascii_case("default") {
        let pins = CameraPins::default();
        match save_camera_pins(nvs, &pins) {
            Ok(()) => {
                update_lcd_camera_status(nvs, "CAMERA", "Pins reset to default");
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("\u{1f504} Camera pins reset to default\n\n{}", pins.status_text()));
            }
            Err(err) => {
                update_lcd_camera_status(nvs, "ERROR", "Camera default save failed");
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("Camera default save failed: {}", err));
            }
        }
        return;
    }

    if let Some(rest) = trimmed.strip_prefix("pins ") {
        let mut pins = load_camera_pins(nvs);
        match parse_camera_pin_args(rest, &mut pins).and_then(|_| save_camera_pins(nvs, &pins)) {
            Ok(()) => {
                update_lcd_camera_status(nvs, "CAMERA", "Pins updated");
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("\u{2705} Camera pins updated\n\n{}", pins.status_text()));
            }
            Err(err) => {
                update_lcd_camera_status(nvs, "ERROR", "Camera pin update failed");
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("Camera pin update failed: {}", err));
            }
        }
        return;
    }

    if trimmed.eq_ignore_ascii_case("selftest") {
        let _ = send_telegram(&cfg.tg_token, chat_id, "📷 準備執行 camera self-test...");
        update_lcd_camera_status(nvs, "CAMERA", "Running self-test");
        let pins = load_camera_pins(nvs);
        match capture_camera_jpeg(&pins) {
            Ok(jpeg) => {
                update_lcd_camera_status(nvs, "CAMERA", &format!("Self-test OK {} KB", jpeg.len() / 1024));
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("\u{2705} Camera self-test OK\nJPEG: {} KB", jpeg.len() / 1024));
            }
            Err(err) => {
                update_lcd_camera_status(nvs, "ERROR", "Camera self-test failed");
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("Camera self-test failed: {}", err));
            }
        }
        return;
    }

    if let Some(rest) = trimmed.strip_prefix("vision") {
        let prompt = rest.trim().trim_start_matches(':').trim();
        let prompt = if prompt.is_empty() {
            "請用繁體中文簡單描述這張照片的重點、人物、物品、背景，若看到文字也請直接說出來"
        } else {
            prompt
        };

        let _ = send_telegram(&cfg.tg_token, chat_id, "📷 準備拍照...");
        update_lcd_camera_status(nvs, "CAMERA", "Vision capture in progress");
        let pins = load_camera_pins(nvs);
        match capture_camera_jpeg(&pins) {
            Ok(jpeg) => {
                let size_kb = jpeg.len() / 1024;
                update_lcd_camera_status(nvs, "CAMERA", &format!("Photo {} KB, analyzing...", size_kb));
                // 1. 先把照片傳給用戶
                let caption = format!("📷 {} KB — 正在分析...", size_kb);
                let _ = send_telegram_camera_jpeg(&cfg.tg_token, chat_id, &jpeg, &caption);
                // 2. 再問 Gemini Vision
                match describe_image_with_gemini(&cfg.gemini_key, state, &jpeg, prompt) {
                    Ok(reply) => {
                        update_lcd_camera_status(nvs, "CAMERA", "Vision OK");
                        // 視覺回覆只送 Telegram、不上 LCD,序列這邊完全看不到內容 ——
                        // 「回太長」「答非所問」這類問題就沒有任何線索可查。印出長度
                        // 長度就好 —— README 承諾聊天內容不進序列 log(畫面裡可能有私人資訊)。
                        info!("Vision reply: {} chars", reply.chars().count());
                        let _ = send_telegram(&cfg.tg_token, chat_id, &reply);
                    }
                    Err(err) => {
                        update_lcd_camera_status(nvs, "ERROR", "Gemini Vision failed");
                        let _ = send_telegram(&cfg.tg_token, chat_id, &format!("Gemini 分析失敗: {}", err));
                    }
                }
            }
            Err(err) => {
                update_lcd_camera_status(nvs, "ERROR", "Camera vision failed");
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("Camera vision failed: {}\n\n請用 /camera 重試或調整 pin 設定", err));
            }
        }
        return;
    }

    // ── MJPEG HTTP stream ──────────────────────────────────────────────────
    if trimmed.eq_ignore_ascii_case("stream stop") || trimmed.eq_ignore_ascii_case("stream off") {
        STREAM_ACTIVE.store(false, Ordering::Relaxed);
        update_lcd_camera_status(nvs, "CAMERA", "Stream stopped");
        let _ = send_telegram(&cfg.tg_token, chat_id, "📡 MJPEG 串流已停止");
        return;
    }

    if trimmed.eq_ignore_ascii_case("stream") || trimmed.starts_with("stream ") {
        if STREAM_ACTIVE.load(Ordering::Relaxed) {
            let _ = send_telegram(&cfg.tg_token, chat_id, "📡 串流已在執行中\n停止: /camera stream stop");
            return;
        }
        let pins = load_camera_pins(nvs);
        let port: u16 = trimmed.strip_prefix("stream ").and_then(|s| s.trim().parse().ok()).unwrap_or(8080);
        // 存取 token 必須用硬體亂數。原本是 now_epoch() ^ (process::id() * 常數),
        // 而 process::id() 在 ESP-IDF 上是 stub(固定值)—— 等於整個 token 只是
        // 「串流啟動當下的 Unix 秒數」的函數,猜幾千次就中。8080 埠除了這個 token
        // 沒有別的保護,猜中就直接看到即時畫面。
        // 同一份程式裡控制台的串流 token 用的就是 esp_random(),兩條路徑要一致。
        let stream_token = format!("{:08x}{:08x}",
            unsafe { esp_idf_svc::sys::esp_random() },
            unsafe { esp_idf_svc::sys::esp_random() });
        start_mjpeg_stream_thread(pins, port, stream_token.clone());
        update_lcd_camera_status(nvs, "CAMERA", &format!("MJPEG :{}  live", port));
        let device_ip = nvs_get(nvs, "wifi_cur_ip").unwrap_or_else(|| "192.168.x.x".to_string());
        let _ = send_telegram(&cfg.tg_token, chat_id,
            &format!("📡 MJPEG 串流已啟動（port {}）\n\n瀏覽器/VLC 開啟：\nhttp://{}:{}/stream?token={}\n\n⚠️ 此連結含存取 token，請勿分享\n\n停止：/camera stream stop", port, device_ip, port, stream_token));
        return;
    }
    // ─────────────────────────────────────────────────────────────────────

    if trimmed.eq_ignore_ascii_case("snap") {
        let _ = send_telegram(&cfg.tg_token, chat_id, "📷 準備拍照...");
        update_lcd_camera_status(nvs, "CAMERA", "Snap in progress");
        let pins = load_camera_pins(nvs);
        match capture_camera_jpeg(&pins) {
            Ok(jpeg) => {
                update_lcd_camera_status(nvs, "CAMERA", &format!("Snap OK {} KB", jpeg.len() / 1024));
                let caption = format!("NANA 拍照完成，JPEG 大小 {} KB", jpeg.len() / 1024);
                let upload_result = send_telegram_camera_jpeg(&cfg.tg_token, chat_id, &jpeg, &caption);
                if let Err(err) = upload_result {
                    update_lcd_camera_status(nvs, "ERROR", "Photo upload failed");
                    let _ = send_telegram(&cfg.tg_token, chat_id, &format!("Photo upload failed after retry: {}", err));
                }
            }
            Err(err) => {
                update_lcd_camera_status(nvs, "ERROR", "Camera snap failed");
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("Camera snap failed: {}", err));
            }
        }
        return;
    }

    let _ = send_telegram(&cfg.tg_token, chat_id, "Usage:\n/camera                  (狀態)\n/camera snap             (拍照並傳照片)\n/camera vision           (拍照+Gemini分析)\n/camera vision <問題>    (拍照+指定問題)\n/camera stream           (MJPEG串流，瀏覽器可看)\n/camera stream stop      (停止串流)\n/camera selftest\n/camera default\n/camera pins pwdn=-1 reset=-1 xclk=15 sda=4 scl=5 d0=11 d1=9 d2=8 d3=10 d4=12 d5=18 d6=17 d7=16 vsync=6 href=7 pclk=13");
}

#[cfg(not(esp_idf_comp_espressif__esp32_camera_enabled))]
fn handle_camera_command(
    cfg: &Config,
    _nvs: &mut EspNvs<NvsDefault>,
    _state: &mut AppState,
    chat_id: i64,
    _args: &str,
) {
    let _ = send_telegram(&cfg.tg_token, chat_id, "Camera component is not enabled in this build.");
}

/// Parse [PC:...] tags from Gemini response and send USB commands.
/// Returns (cleaned_reply, had_pc_commands).
/// Max 5 PC commands per AI reply to prevent runaway loops.
fn parse_pc_tags(reply: &str, chat_id: i64) -> (String, bool) {
    const MAX_PC_CMDS_PER_REPLY: usize = 5;
    let mut clean = reply.to_string();
    let mut applied = false;
    let mut cmd_count: usize = 0;

    while let Some(start) = clean.find("[PC:") {
        if cmd_count >= MAX_PC_CMDS_PER_REPLY {
            warn!("PC command limit ({}) reached, skipping remaining [PC:] tags", MAX_PC_CMDS_PER_REPLY);
            // Strip remaining [PC:...] tags without executing
            while let Some(s) = clean.find("[PC:") {
                if let Some(e) = clean[s..].find(']') {
                    clean = format!("{}{}", &clean[..s], clean[s+e+1..].trim_start());
                } else {
                    break;
                }
            }
            break;
        }
        if let Some(end) = clean[start..].find(']') {
            let tag = &clean[start+4..start+end];
            // Split into cmd and raw_arg at FIRST colon
            let (cmd, raw_arg) = if let Some(pos) = tag.find(':') {
                (&tag[..pos], &tag[pos+1..])
            } else {
                (tag, "")
            };

            let args_json = match cmd {
                "shell" => serde_json::json!({"command": raw_arg}),
                "file_read" => serde_json::json!({"path": raw_arg}),
                "file_list" => serde_json::json!({"path": if raw_arg.is_empty() { "." } else { raw_arg }}),
                "file_write" => {
                    let parts: Vec<&str> = raw_arg.splitn(2, ">>>").collect();
                    serde_json::json!({"path": parts.first().unwrap_or(&""), "content": parts.get(1).unwrap_or(&"")})
                }
                "open" => serde_json::json!({"target": raw_arg}),
                "screenshot" => serde_json::json!({}),
                "analyze_image" => serde_json::json!({"path": raw_arg}),
                "scan_inbox" => serde_json::json!({}),
                "status" => serde_json::json!({}),
                "excel_read" => serde_json::json!({"action": "read", "path": raw_arg}),
                "excel_write" => {
                    let parts: Vec<&str> = raw_arg.splitn(3, ">>>").collect();
                    serde_json::json!({"action": "write", "path": parts.first().unwrap_or(&""), "cell": parts.get(1).unwrap_or(&"A1"), "value": parts.get(2).unwrap_or(&"")})
                }
                "email_send" => {
                    let parts: Vec<&str> = raw_arg.splitn(3, ">>>").collect();
                    serde_json::json!({"action": "send", "to": parts.first().unwrap_or(&""), "subject": parts.get(1).unwrap_or(&""), "body": parts.get(2).unwrap_or(&"")})
                }
                "clipboard_get" => serde_json::json!({"action": "get"}),
                "clipboard_set" => serde_json::json!({"action": "set", "text": raw_arg}),
                "process_list" => serde_json::json!({"action": "list"}),
                "click" => {
                    let coords: Vec<&str> = raw_arg.split(',').collect();
                    let x: i32 = coords.first().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
                    let y: i32 = coords.get(1).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
                    serde_json::json!({"x": x, "y": y, "button": "left"})
                }
                "type" => serde_json::json!({"text": raw_arg}),
                "hotkey" => {
                    let keys: Vec<&str> = raw_arg.split('+').map(|s| s.trim()).collect();
                    serde_json::json!({"keys": keys})
                }
                "mouse_move" => {
                    let coords: Vec<&str> = raw_arg.split(',').collect();
                    let x: i32 = coords.first().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
                    let y: i32 = coords.get(1).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
                    serde_json::json!({"x": x, "y": y})
                }
                "scroll" => {
                    let amount: i32 = raw_arg.trim().parse().unwrap_or(3);
                    serde_json::json!({"amount": amount})
                }
                "find_window" => serde_json::json!({"title": raw_arg}),
                "focus_window" => serde_json::json!({"title": raw_arg}),
                _ => serde_json::json!({"command": raw_arg}),
            };

            // Map compound command names to the actual driver command
            let driver_cmd = if cmd.starts_with("excel") { "excel" }
                else if cmd.starts_with("email") { "email" }
                else if cmd.starts_with("clipboard") { "clipboard" }
                else if cmd.starts_with("process") { "process" }
                else if cmd == "mouse_move" { "mouse_move" }
                else if cmd == "find_window" { "find_window" }
                else if cmd == "focus_window" { "focus_window" }
                else if cmd == "analyze_image" { "analyze_image" }
                else if cmd == "scan_inbox" { "scan_inbox" }
                else { cmd };

            // Enforce PC Safe Mode: block dangerous AI-generated commands
            if !pc_cmd_allowed(driver_cmd, true) {
                warn!("PC Safe Mode: blocked AI command [{}]", driver_cmd);
                clean = format!("{}{}", &clean[..start], clean[start+end+1..].trim_start());
                continue;
            }

            usb_send_pc_cmd(driver_cmd, &args_json, chat_id);
            applied = true;
            cmd_count += 1;

            clean = format!("{}{}", &clean[..start], clean[start+end+1..].trim_start());
        } else {
            break;
        }
    }

    (clean.trim().to_string(), applied)
}

// ===== Config =====

struct Config {
    wifi: Vec<(String, String)>,  // up to 5 (ssid, pass) pairs
    tg_token: String,
    gemini_key: String,
    chat_id: String,
}

#[cfg(esp_idf_comp_espressif__esp32_camera_enabled)]
const CAMERA_NVS_KEY: &str = "camera_pins";

#[cfg(esp_idf_comp_espressif__esp32_camera_enabled)]
#[derive(Clone, Debug, Serialize, Deserialize)]
struct CameraPins {
    pwdn: i32,
    reset: i32,
    xclk: i32,
    sda: i32,
    scl: i32,
    d0: i32,
    d1: i32,
    d2: i32,
    d3: i32,
    d4: i32,
    d5: i32,
    d6: i32,
    d7: i32,
    vsync: i32,
    href: i32,
    pclk: i32,
}

#[cfg(esp_idf_comp_espressif__esp32_camera_enabled)]
impl Default for CameraPins {
    fn default() -> Self {
        Self {
            pwdn: -1,
            reset: -1,
            xclk: 15,
            sda: 4,
            scl: 5,
            d0: 11,
            d1: 9,
            d2: 8,
            d3: 10,
            d4: 12,
            d5: 18,
            d6: 17,
            d7: 16,
            vsync: 6,
            href: 7,
            pclk: 13,
        }
    }
}

#[cfg(esp_idf_comp_espressif__esp32_camera_enabled)]
impl CameraPins {
    fn apply_setting(&mut self, key: &str, value: i32) -> Result<()> {
        match key {
            "pwdn" => self.pwdn = value,
            "reset" => self.reset = value,
            "xclk" => self.xclk = value,
            "sda" => self.sda = value,
            "scl" => self.scl = value,
            "d0" => self.d0 = value,
            "d1" => self.d1 = value,
            "d2" => self.d2 = value,
            "d3" => self.d3 = value,
            "d4" => self.d4 = value,
            "d5" => self.d5 = value,
            "d6" => self.d6 = value,
            "d7" => self.d7 = value,
            "vsync" => self.vsync = value,
            "href" => self.href = value,
            "pclk" => self.pclk = value,
            _ => bail!("unknown camera pin field: {}", key),
        }
        Ok(())
    }

    fn status_text(&self) -> String {
        format!(
            "OV3660 pins\npwdn={} reset={} xclk={} sda={} scl={}\nd0={} d1={} d2={} d3={} d4={} d5={} d6={} d7={}\nvsync={} href={} pclk={}",
            self.pwdn,
            self.reset,
            self.xclk,
            self.sda,
            self.scl,
            self.d0,
            self.d1,
            self.d2,
            self.d3,
            self.d4,
            self.d5,
            self.d6,
            self.d7,
            self.vsync,
            self.href,
            self.pclk,
        )
    }
}

#[cfg(esp_idf_comp_espressif__esp32_camera_enabled)]
fn load_camera_pins(nvs: &EspNvs<NvsDefault>) -> CameraPins {
    nvs_get(nvs, CAMERA_NVS_KEY)
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

#[cfg(esp_idf_comp_espressif__esp32_camera_enabled)]
fn save_camera_pins(nvs: &mut EspNvs<NvsDefault>, pins: &CameraPins) -> Result<()> {
    let raw = serde_json::to_string(pins)?;
    nvs.set_str(CAMERA_NVS_KEY, &raw)?;
    Ok(())
}

#[cfg(esp_idf_comp_espressif__esp32_camera_enabled)]
fn parse_camera_pin_args(input: &str, pins: &mut CameraPins) -> Result<()> {
    for token in input.split_whitespace() {
        let (key, value) = token
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("bad camera pin token: {}", token))?;
        let parsed = value
            .parse::<i32>()
            .map_err(|_| anyhow::anyhow!("invalid pin value for {}", key))?;
        pins.apply_setting(key, parsed)?;
    }
    Ok(())
}

// ===== Reminder System =====

#[derive(Clone, Debug)]
struct Reminder {
    id: u32,
    rtype: ReminderType,
    interval_secs: u64,
    next_trigger: u64,
    hour: u32,    // for Daily/Once: original scheduled hour
    minute: u32,  // for Daily/Once: original scheduled minute
    message: String,
    active: bool,
}

#[derive(Clone, Debug)]
struct AutoTask {
    id: u32,
    interval_secs: u64,
    next_trigger: u64,
    action: String,   // "camera"
    prompt: String,
    speak: bool,
    active: bool,
    /// 連續有幾「輪」完全失敗(重試也用盡)。純記憶體欄位。
    /// 監控類任務最危險的失效是**安靜地停止運作** —— 相機被遮住、鏡頭髒了、
    /// 網路斷了,任務每輪都失敗但使用者什麼都收不到,還以為一切正常。
    /// 連續失敗到門檻就主動通知一次,把「靜默失效」變成「明確告警」。
    consecutive_failures: u8,
    /// 這一輪還剩幾次重試機會。純記憶體欄位,不進 NVS(NVS 格式不變)。
    /// 為什麼需要:AI 呼叫有每分鐘 5 次的速率上限,使用者剛好在拍照時間點附近
    /// 講了幾句話,額度就被吃掉 → 那一輪整個跳過,要等下一個間隔。
    /// 使用者觀察到的「每 5 分鐘拍照,有時候沒拍到」就是這個。
    retries_left: u8,
}

#[derive(Clone, Debug)]
enum ReminderType {
    Interval,
    Daily,
    Once,
}

struct AppState {
    reminders: Vec<Reminder>,
    next_reminder_id: u32,
    auto_tasks: Vec<AutoTask>,
    next_autotask_id: u32,
    tokens_in: u64,
    tokens_out: u64,
    requests: u64,
    history: Vec<(String, String)>,
    boot_time: u64,
    memories: Vec<(String, String)>,
    diag: DiagBuffer,
    skills: Vec<Skill>,
    last_tokens_in: u64,
    last_tokens_out: u64,
    current_model: String,
    voice_mode: String,
    wake_phrase: String,
    wake_enabled: bool,
    last_wake_check: u64,
    last_wake_trigger: u64,
    last_briefing_day: i64,
}

impl AppState {
    fn new(now: u64) -> Self {
        Self {
            reminders: Vec::new(),
            next_reminder_id: 1,
            auto_tasks: Vec::new(),
            next_autotask_id: 1,
            tokens_in: 0,
            tokens_out: 0,
            requests: 0,
            history: Vec::new(),
            boot_time: now,
            memories: Vec::new(),
            diag: DiagBuffer::new(),
            skills: Vec::new(),
            last_tokens_in: 0,
            last_tokens_out: 0,
            current_model: DEFAULT_GEMINI_MODEL.to_string(),
            voice_mode: "normal".to_string(),
            wake_phrase: DEFAULT_WAKE_PHRASE.to_string(),
            wake_enabled: false,
            last_wake_check: 0,
            last_wake_trigger: 0,
            last_briefing_day: -1,
        }
    }

    fn add_reminder(&mut self, rtype: ReminderType, interval_secs: u64,
                    hour: u32, minute: u32, message: &str, now: u64) -> u32 {
        let id = self.next_reminder_id;
        self.next_reminder_id += 1;

        let next_trigger = match rtype {
            ReminderType::Interval => now + interval_secs,
            ReminderType::Daily | ReminderType::Once => {
                next_daily_trigger(hour, minute, now)
            }
        };

        if self.reminders.len() >= MAX_REMINDERS {
            self.reminders.retain(|r| r.active);
        }

        self.reminders.push(Reminder {
            id, rtype, interval_secs, next_trigger,
            hour, minute,
            message: message.to_string(), active: true,
        });
        id
    }

    fn remove_reminder(&mut self, id: u32) -> bool {
        if let Some(r) = self.reminders.iter_mut().find(|r| r.id == id) {
            r.active = false;
            true
        } else {
            false
        }
    }

    fn check_reminders(&mut self, now: u64) -> Vec<String> {
        let mut fired = Vec::new();
        for r in &mut self.reminders {
            if !r.active || now < r.next_trigger {
                continue;
            }
            fired.push(format!("[Reminder] {}", r.message));
            match r.rtype {
                ReminderType::Interval => {
                    r.next_trigger = now + r.interval_secs;
                }
                ReminderType::Daily => {
                    // 要「補到超過現在」而不是只加一天。
                    //
                    // 裝置關機幾天再開機時 next_trigger 會落後好幾天,只加一天之後
                    // 仍然是過去 -> 下一次 30 秒檢查又觸發 -> 再加一天 -> 又觸發……
                    // 每 30 秒連發一次直到追上今天。使用者看到的就是「提醒一直跳出來」。
                    // 落後 N 天就連跳 N 次,而且訊息一模一樣,看起來像壞掉。
                    //
                    // 補到未來為止,只發這一次(當天該提醒的那一次),不補發積欠的。
                    while r.next_trigger <= now {
                        r.next_trigger += 86400;
                    }
                }
                ReminderType::Once => {
                    r.active = false;
                }
            }
        }
        fired
    }

    fn list_reminders(&self) -> String {
        let active: Vec<&Reminder> = self.reminders.iter().filter(|r| r.active).collect();
        if active.is_empty() {
            return "📭 沒有提醒\n\n新增：\n  /remind 5m 訊息 — 每 N 分鐘\n  /remind 8:00 訊息 — 每天 8 點".to_string();
        }
        let now = now_epoch();
        let mut s = format!("📋 提醒清單 ({} 筆):\n", active.len());
        for r in &active {
            let type_str = match r.rtype {
                ReminderType::Interval => {
                    let mins = r.interval_secs / 60;
                    let secs = r.interval_secs % 60;
                    if secs == 0 { format!("每{}分", mins) } else { format!("每{}分{}秒", mins, secs) }
                }
                ReminderType::Daily => {
                    let (h, m) = epoch_to_hhmm(r.next_trigger);
                    format!("每天{:02}:{:02}", h, m)
                }
                ReminderType::Once => {
                    let (h, m) = epoch_to_hhmm(r.next_trigger);
                    format!("一次{:02}:{:02}", h, m)
                }
            };
            let countdown = if r.next_trigger > now {
                let rem = r.next_trigger - now;
                if rem >= 3600 { format!("{}時{}分後", rem/3600, (rem%3600)/60) }
                else if rem >= 60 { format!("{}分{}秒後", rem/60, rem%60) }
                else { format!("{}秒後", rem) }
            } else {
                "已觸發".to_string()
            };
            let short = if r.message.chars().count() > 20 {
                format!("{}...", r.message.chars().take(20).collect::<String>())
            } else { r.message.clone() };
            s.push_str(&format!("  #{} {} — {}\n     💬{}\n", r.id, type_str, countdown, short));
        }
        s.push_str("\n────────────────\n");
        s.push_str("🗑️ /remind del [ID編號]\n");
        s.push_str("🧹 清除全部：/reminders clear");
        s
    }

    fn add_history(&mut self, role: &str, text: &str) {
        self.history.push((role.to_string(), text.to_string()));
        if self.history.len() > MAX_HISTORY {
            self.history.remove(0);
        }
    }

    fn token_report(&self) -> String {
        let total = self.tokens_in + self.tokens_out;
        format!(
            "Token Usage Report\n\
             Model: {}\n\
             Voice Mode: {}\n\
             Wake: {} ({})\n\
             Input: {} tokens\n\
             Output: {} tokens\n\
             Total: {} tokens\n\
             Requests: {}\n\
             Avg/request: {} tokens",
            self.current_model,
            self.voice_mode,
            if self.wake_enabled { "on" } else { "off" },
            self.wake_phrase,
            self.tokens_in,
            self.tokens_out,
            total,
            self.requests,
            if self.requests > 0 { total / self.requests } else { 0 }
        )
    }
}

// ===== Time Helpers =====

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn epoch_to_hhmm(epoch: u64) -> (u32, u32) {
    let local = epoch as i64 + TZ_OFFSET;
    let day_secs = ((local % 86400) + 86400) as u32 % 86400;
    (day_secs / 3600, (day_secs % 3600) / 60)
}

fn next_daily_trigger(hour: u32, minute: u32, now: u64) -> u64 {
    let local_now = now as i64 + TZ_OFFSET;
    let day_start = local_now - (local_now % 86400);
    let target = day_start + (hour as i64) * 3600 + (minute as i64) * 60;
    let target_utc = target - TZ_OFFSET;
    if target_utc as u64 <= now {
        (target_utc + 86400) as u64
    } else {
        target_utc as u64
    }
}

// ===== Telegram Types =====

#[derive(Debug, Deserialize)]
struct TgResponse {
    ok: bool,
    result: Option<Vec<TgUpdate>>,
}

#[derive(Debug, Deserialize)]
struct TgUpdate {
    update_id: i64,
    message: Option<TgMessage>,
}

#[derive(Debug, Deserialize)]
struct TgMessage {
    chat: TgChat,
    from: Option<TgUser>,
    text: Option<String>,
    caption: Option<String>,
    voice: Option<TgVoice>,
    audio: Option<TgAudio>,
    #[serde(default)]
    photo: Vec<TgPhotoSize>,
    document: Option<TgDocument>,
}

#[derive(Debug, Deserialize)]
struct TgChat {
    id: i64,
    /// "private" / "group" / "supergroup" / "channel"
    #[serde(rename = "type", default)]
    kind: String,
}

#[derive(Debug, Deserialize)]
struct TgUser {
    id: i64,
}

fn ensure_telegram_api_ok(body: &str) -> Result<()> {
    let value: serde_json::Value = serde_json::from_str(body)?;
    if value.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        return Ok(());
    }
    let desc = value
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown telegram error");
    bail!("Telegram API error: {}", desc)
}

#[derive(Debug, Deserialize)]
struct TgVoice {
    file_id: String,
    #[allow(dead_code)]
    duration: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct TgAudio {
    file_id: String,
    mime_type: Option<String>,
    #[allow(dead_code)]
    duration: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct TgPhotoSize {
    file_id: String,
    #[allow(dead_code)]
    width: Option<u32>,
    #[allow(dead_code)]
    height: Option<u32>,
    #[allow(dead_code)]
    file_size: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct TgDocument {
    file_id: String,
    file_name: Option<String>,
    mime_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TgFileResponse {
    ok: bool,
    result: Option<TgFile>,
}

#[derive(Debug, Deserialize)]
struct TgFile {
    file_path: Option<String>,
}

// ===== Gemini Types =====

#[derive(Serialize)]
struct GeminiRequest {
    #[serde(rename = "systemInstruction")]
    system_instruction: GeminiSysInstr,
    contents: Vec<GeminiContent>,
    #[serde(rename = "generationConfig")]
    generation_config: GenConfig,
}

#[derive(Serialize)]
struct GeminiSysInstr {
    parts: Vec<GeminiTextPart>,
}

#[derive(Serialize)]
struct GenConfig {
    #[serde(rename = "maxOutputTokens")]
    max_output_tokens: u32,
    temperature: f32,
}

#[derive(Serialize)]
struct GeminiContent {
    role: String,
    parts: Vec<GeminiPartOut>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum GeminiPartOut {
    Text { text: String },
    InlineData { inline_data: InlineData },
}

#[derive(Serialize)]
struct InlineData {
    mime_type: String,
    data: String,
}

#[derive(Serialize)]
struct GeminiTextPart {
    text: String,
}

#[derive(Debug, Deserialize)]
struct GeminiResponse {
    candidates: Option<Vec<GeminiCandidate>>,
    #[serde(rename = "usageMetadata")]
    usage_metadata: Option<UsageMetadata>,
    #[serde(rename = "promptFeedback")]
    prompt_feedback: Option<PromptFeedback>,
}

#[derive(Debug, Deserialize)]
struct PromptFeedback {
    #[serde(rename = "blockReason")]
    block_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UsageMetadata {
    #[serde(rename = "promptTokenCount")]
    prompt_token_count: Option<u64>,
    #[serde(rename = "candidatesTokenCount")]
    candidates_token_count: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidate {
    content: Option<GeminiRespContent>,
    #[serde(rename = "finishReason")]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiRespContent {
    parts: Option<Vec<GeminiRespPart>>,
}

#[derive(Debug, Deserialize)]
struct GeminiRespPart {
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TtsProxyJsonResponse {
    ok: bool,
    audio_b64: Option<String>,
    error: Option<String>,
}

// ===== NVS Helpers =====

fn nvs_get(nvs: &EspNvs<NvsDefault>, key: &str) -> Option<String> {
    let mut buf = [0u8; 256];
    nvs.get_str(key, &mut buf)
        .ok()
        .flatten()
        .map(|s| s.trim_end_matches('\0').to_string())
}

fn nvs_get_bool(nvs: &EspNvs<NvsDefault>, key: &str) -> Option<bool> {
    nvs_get(nvs, key).and_then(|value| match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    })
}

fn nvs_get_i64(nvs: &EspNvs<NvsDefault>, key: &str) -> Option<i64> {
    nvs_get(nvs, key).and_then(|value| value.trim().parse::<i64>().ok())
}

fn load_last_telegram_update_id(nvs: &EspNvs<NvsDefault>) -> i64 {
    nvs_get_i64(nvs, TG_LAST_UPDATE_ID_KEY).unwrap_or(0)
}

fn save_last_telegram_update_id(nvs: &mut EspNvs<NvsDefault>, update_id: i64) {
    if let Err(err) = nvs.set_str(TG_LAST_UPDATE_ID_KEY, &update_id.to_string()) {
        warn!("Persist telegram update id failed: {}", err);
    }
}

fn load_tts_proxy_url(nvs: &EspNvs<NvsDefault>) -> Option<String> {
    nvs_get(nvs, TTS_PROXY_URL_KEY)
}

fn load_tts_proxy_voice(nvs: &EspNvs<NvsDefault>) -> String {
    nvs_get(nvs, TTS_PROXY_VOICE_KEY)
        .unwrap_or_else(|| "zh-TW-HsiaoChenNeural".to_string())
}

/// Read one line from UART0 using low-level esp-idf uart_read_bytes.
/// Returns the line (without newline) or empty string on timeout.
fn uart0_read_line(timeout_ms: u32) -> String {
    let mut buf = Vec::with_capacity(512);
    let byte_timeout = if timeout_ms > 200 { 200 } else { timeout_ms };
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms as u64);
    loop {
        if std::time::Instant::now() > deadline {
            break;
        }
        let mut byte = [0u8; 1];
        let n = unsafe {
            esp_idf_svc::sys::uart_read_bytes(
                0, // UART0
                byte.as_mut_ptr() as *mut _,
                1,
                byte_timeout / 10, // ticks (portTICK_PERIOD_MS = 10ms on default)
            )
        };
        if n > 0 {
            if byte[0] == b'\n' || byte[0] == b'\r' {
                if !buf.is_empty() {
                    break;
                }
                // skip leading CR/LF
                continue;
            }
            buf.push(byte[0]);
            if buf.len() >= 4096 {
                break; // safety limit
            }
        }
    }
    String::from_utf8_lossy(&buf).to_string()
}

/// Wait for JSON provisioning via USB Serial when NVS is empty (first boot).
/// Expected format: {"WIFI_SSID":"...","TG_TOKEN":"...","GEMINI_KEY":"...","CHAT_ID":"...",...}
fn serial_provision(nvs: &mut EspNvs<NvsDefault>) -> Result<()> {
    warn!("NVS config incomplete — entering Serial provisioning mode");
    warn!("Send JSON config via USB Serial (or run provision.ps1)");

    // Install UART0 driver so uart_read_bytes works
    let uart_installed = unsafe {
        esp_idf_svc::sys::uart_driver_install(
            0,    // UART0
            1024, // RX buffer
            0,    // TX buffer (not needed — println! uses VFS)
            0,    // queue size
            std::ptr::null_mut(), // queue handle
            0,    // intr alloc flags
        )
    };
    if uart_installed != 0 {
        warn!("uart_driver_install returned {} (may already be installed)", uart_installed);
    }

    // Print a machine-readable marker so provision.ps1 can detect readiness
    println!("{{\"provision\":\"ready\"}}");

    let deadline = std::time::Instant::now() + Duration::from_secs(300); // 5 min timeout

    loop {
        if std::time::Instant::now() > deadline {
            bail!("Provisioning timeout — no config received in 5 minutes");
        }
        let line = uart0_read_line(2000);
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.starts_with('{') {
            continue;
        }
        let obj: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                warn!("Invalid JSON: {}", e);
                println!("{{\"provision\":\"error\",\"msg\":\"invalid JSON\"}}");
                continue;
            }
        };
        // Store all recognised keys into NVS
        let keys = [
            ("WIFI_SSID", "wifi_ssid"),
            ("WIFI_PASS", "wifi_pass"),
            ("WIFI_SSID2", "wifi_ssid2"),
            ("WIFI_PASS2", "wifi_pass2"),
            ("WIFI_SSID3", "wifi_ssid3"),
            ("WIFI_PASS3", "wifi_pass3"),
            ("WIFI_SSID4", "wifi_ssid4"),
            ("WIFI_PASS4", "wifi_pass4"),
            ("WIFI_SSID5", "wifi_ssid5"),
            ("WIFI_PASS5", "wifi_pass5"),
            ("TG_TOKEN", "tg_token"),
            ("GEMINI_KEY", "gemini_key"),
            ("CHAT_ID", "chat_id"),
            ("TTS_PROXY_URL", TTS_PROXY_URL_KEY),
            ("TTS_PROXY_VOICE", TTS_PROXY_VOICE_KEY),
        ];
        let mut stored = 0u32;
        for (json_key, nvs_key) in keys {
            if let Some(val) = obj.get(json_key).and_then(|v| v.as_str()) {
                if !val.is_empty() {
                    let _ = nvs.set_str(nvs_key, val);
                    stored += 1;
                }
            }
        }
        // Validate required keys are now present。CHAT_ID 不再是必填 —
        // 綁定改在連網後由控制台的一鍵綁定流程自動完成。
        let has_wifi = nvs_get(nvs, "wifi_ssid").is_some();
        let has_tg   = nvs_get(nvs, "tg_token").is_some();
        let has_gem  = nvs_get(nvs, "gemini_key").is_some();
        if has_wifi && has_tg && has_gem {
            info!("Provisioning complete — {} keys stored", stored);
            println!("{{\"provision\":\"ok\",\"stored\":{}}}", stored);
            return Ok(());
        }
        let missing: Vec<&str> = [
            (!has_wifi, "WIFI_SSID"),
            (!has_tg,   "TG_TOKEN"),
            (!has_gem,  "GEMINI_KEY"),
        ].iter().filter(|(m, _)| *m).map(|(_, n)| *n).collect();
        warn!("Provisioning incomplete, still missing: {:?}", missing);
        println!("{{\"provision\":\"incomplete\",\"missing\":{:?}}}", missing);
    }
}

fn load_config(nvs: &mut EspNvs<NvsDefault>) -> Result<Config> {
    // Check if core secrets exist in NVS; if not, enter serial provisioning.
    // chat_id 不在此列:改為連上網路後,使用者在控制台點「加好友並綁定」,
    // 由第一則傳進來的 Telegram 訊息自動學會 chat_id,不必手動查/填數字 ID。
    let needs_provision = nvs_get(nvs, "wifi_ssid").is_none()
        || nvs_get(nvs, "tg_token").is_none()
        || nvs_get(nvs, "gemini_key").is_none();

    if needs_provision {
        // 不在這裡阻塞等序列輸入 — 交給 main 進 SoftAP 配網模式,
        // 序列配網會在該模式下以背景執行緒同時提供(相容 USB 設定工具)。
        bail!("UNPROVISIONED");
    }

    let wifi_ssid = nvs_get(nvs, "wifi_ssid")
        .ok_or_else(|| anyhow::anyhow!("WIFI_SSID not set"))?;

    let wifi_pass = nvs_get(nvs, "wifi_pass")
        .unwrap_or_default();

    // Load up to 5 WiFi networks
    let mut wifi = vec![(wifi_ssid, wifi_pass)];
    for i in 2..=5u8 {
        let sk = format!("wifi_ssid{}", i);
        let pk = format!("wifi_pass{}", i);
        let ssid = nvs_get(nvs, &sk).unwrap_or_default();
        let pass = nvs_get(nvs, &pk).unwrap_or_default();
        wifi.push((ssid, pass));
    }

    let tg_token = nvs_get(nvs, "tg_token")
        .ok_or_else(|| anyhow::anyhow!("TG_TOKEN not set"))?;

    let gemini_key = nvs_get(nvs, "gemini_key")
        .ok_or_else(|| anyhow::anyhow!("GEMINI_KEY not set"))?;

    // 空字串 = 尚未綁定;主迴圈第一次收到 Telegram 訊息時自動學會並存回 NVS。
    let chat_id = nvs_get(nvs, "chat_id").unwrap_or_default();

    Ok(Config { wifi, tg_token, gemini_key, chat_id })
}

// ===== BLE Provisioning (NUS — Nordic UART Service) =====

/// NVS key→value config commands handled over BLE.
/// Protocol:
///   Phone→ESP: "AUTH <pin>"         → "OK" / "FAIL"
///   Phone→ESP: "GET <nvs_key>"      → "OK <value>"  (masked for secrets)
///   Phone→ESP: "SET <nvs_key> <val>"→ "OK"
///   Phone→ESP: "LIST"               → comma-separated key list
///   Phone→ESP: "SAVE"               → "OK RESTART" + reboot
///   Phone→ESP: "PING"               → "PONG SerialSoul vX.Y.Z"
fn start_ble_provisioning(nvs_part: EspDefaultNvsPartition) {
    // BLE setup runs inline — NimBLE owns all objects via Arc/static refs,
    // callbacks run on NimBLE's internal task, no need for a dedicated thread.
    info!("BLE: init NimBLE...");

    // Read BLE auth pin from NVS (allow password change via BLE SET ble_pin).
    // No firmware-constant fallback: if NVS is somehow unavailable, generate an
    // ephemeral random PIN (logged) rather than something an attacker can look up.
    let ble_pin = {
        match EspNvs::new(nvs_part.clone(), NVS_NS, true) {
            Ok(mut nvs) => ensure_ble_pin(&mut nvs),
            Err(e) => {
                warn!("BLE: NVS unavailable ({}), using ephemeral random PIN", e);
                let pin = format!("{:06}", unsafe { esp_idf_svc::sys::esp_random() } % 1_000_000);
                info!("BLE: ephemeral PIN: {}", pin);
                pin
            }
        }
    };
    let ble_pin = Arc::new(Mutex::new(ble_pin));

    let device = BLEDevice::take();
    device
        .security()
        .set_auth(esp32_nimble::enums::AuthReq::empty())
        .set_io_cap(esp32_nimble::enums::SecurityIOCap::NoInputNoOutput);

    let server = device.get_server();
    let authed: Arc<std::sync::atomic::AtomicBool> =
        Arc::new(std::sync::atomic::AtomicBool::new(false));

    let authed_conn = authed.clone();
    server.on_connect(move |_srv, _desc| {
        authed_conn.store(false, Ordering::SeqCst);
        info!("BLE: client connected");
    });
    let authed_disc = authed.clone();
    server.on_disconnect(move |_desc, _reason| {
        authed_disc.store(false, Ordering::SeqCst);
        info!("BLE: client disconnected");
    });

    let service = server.create_service(uuid128!(NUS_SERVICE_UUID));

    // TX: ESP -> Phone (notify)
    let tx_char = service.lock().create_characteristic(
        uuid128!(NUS_TX_UUID),
        NimbleProperties::NOTIFY,
    );
    tx_char.lock().set_value(b"SerialSoul ready");

    // RX: Phone -> ESP (write)
    let tx = tx_char.clone();
    let authed_rx = authed.clone();
    let nvs_part_inner = nvs_part.clone();
    let ble_pin_rx = ble_pin.clone();
    service.lock().create_characteristic(
                uuid128!(NUS_RX_UUID),
                NimbleProperties::WRITE | NimbleProperties::WRITE_NO_RSP,
            ).lock().on_write(move |args| {
                let raw = String::from_utf8_lossy(args.recv_data()).to_string();
                let cmd = raw.trim();
                if cmd.is_empty() { return; }
                info!("BLE RX: {} bytes", cmd.len());

                // helper: send response via TX notify
                let respond = |msg: &str| {
                    tx.lock().set_value(msg.as_bytes());
                    tx.lock().notify();
                };

                if cmd.eq_ignore_ascii_case("PING") {
                    respond(&format!("PONG SerialSoul v{}", FW_VERSION));
                    return;
                }

                if let Some(pin) = cmd.strip_prefix("AUTH ").or_else(|| cmd.strip_prefix("auth ")) {
                    // Brute-force guard: after BLE_AUTH_MAX_FAILS consecutive
                    // failures, lock AUTH with escalating backoff (30s → 5min).
                    // Uses uptime (monotonic) — wall clock may not be NTP-synced yet.
                    let now_s = (unsafe { esp_idf_svc::sys::esp_timer_get_time() } / 1_000_000) as u32;
                    let lock_until = BLE_AUTH_LOCK_UNTIL.load(Ordering::SeqCst);
                    if now_s < lock_until {
                        respond(&format!("LOCKED {}", lock_until - now_s));
                        warn!("BLE: auth locked for {}s more", lock_until - now_s);
                        return;
                    }
                    let current_pin = ble_pin_rx.lock().unwrap().clone();
                    if pin.trim() == current_pin {
                        BLE_AUTH_FAILS.store(0, Ordering::SeqCst);
                        authed_rx.store(true, Ordering::SeqCst);
                        respond("OK");
                        info!("BLE: auth OK");
                    } else {
                        let fails = BLE_AUTH_FAILS.fetch_add(1, Ordering::SeqCst) + 1;
                        if fails >= BLE_AUTH_MAX_FAILS {
                            let over = fails - BLE_AUTH_MAX_FAILS;
                            let lock_secs = core::cmp::min(30u32 << over.min(4), 300);
                            BLE_AUTH_LOCK_UNTIL.store(now_s.saturating_add(lock_secs), Ordering::SeqCst);
                            respond(&format!("LOCKED {}", lock_secs));
                            warn!("BLE: auth FAIL #{} → locked {}s", fails, lock_secs);
                        } else {
                            respond("FAIL");
                            warn!("BLE: auth FAIL #{}", fails);
                        }
                    }
                    return;
                }

                if !authed_rx.load(Ordering::SeqCst) {
                    respond("NEED_AUTH");
                    return;
                }

                let nvs_handle = match EspNvs::new(nvs_part_inner.clone(), NVS_NS, true) {
                    Ok(h) => h,
                    Err(e) => { respond(&format!("ERR NVS: {}", e)); return; }
                };

                if cmd.eq_ignore_ascii_case("LIST") {
                    respond("wifi_ssid,wifi_pass,wifi_ssid2,wifi_pass2,wifi_ssid3,wifi_pass3,\
                             tg_token,chat_id,gemini_key,model_pref,voice_mode,wake_phrase,ble_pin");
                    return;
                }

                if let Some(key) = cmd.strip_prefix("GET ").or_else(|| cmd.strip_prefix("get ")) {
                    let key = key.trim();
                    let val = if key == "ble_pin" {
                        ble_pin_rx.lock().unwrap().clone()
                    } else {
                        nvs_get(&nvs_handle, key).unwrap_or_default()
                    };
                    let masked = match key {
                        "tg_token" | "gemini_key" | "wifi_pass" | "wifi_pass2" | "wifi_pass3"
                        | "wifi_pass4" | "wifi_pass5" | "ble_pin" => {
                            if val.len() > 8 {
                                format!("{}...{}", str_prefix(&val, 4), str_suffix(&val, 4))
                            } else if val.is_empty() {
                                "(empty)".to_string()
                            } else {
                                "****".to_string()
                            }
                        }
                        _ => val,
                    };
                    respond(&format!("OK {}", masked));
                    return;
                }

                if let Some(rest) = cmd.strip_prefix("SET ").or_else(|| cmd.strip_prefix("set ")) {
                    let rest = rest.trim();
                    if let Some((key, val)) = rest.split_once(' ') {
                        let key = key.trim();
                        let val = val.trim();
                        // Validate ble_pin: minimum 6 characters
                        if key == "ble_pin" && val.len() < 6 {
                            respond("ERR ble_pin 至少 6 字元");
                            return;
                        }
                        match EspNvs::new(nvs_part_inner.clone(), NVS_NS, true) {
                            Ok(mut nvs_w) => match nvs_w.set_str(key, val) {
                                Ok(_) => {
                                    info!("BLE SET {}=<{} chars>", key, val.len());
                                    // Update runtime BLE pin if changed
                                    if key == "ble_pin" {
                                        if let Ok(mut pin) = ble_pin_rx.lock() {
                                            *pin = val.to_string();
                                        }
                                        info!("BLE: auth pin updated (runtime + NVS)");
                                    }
                                    respond("OK");
                                }
                                Err(e) => respond(&format!("ERR {}", e)),
                            },
                            Err(e) => respond(&format!("ERR NVS: {}", e)),
                        }
                    } else {
                        respond("ERR SET <key> <value>");
                    }
                    return;
                }

                if cmd.eq_ignore_ascii_case("SAVE") {
                    respond("OK RESTART");
                    std::thread::sleep(Duration::from_millis(300));
                    unsafe { esp_idf_svc::sys::esp_restart(); }
                }

                // ── STATUS: device info ──
                if cmd.eq_ignore_ascii_case("STATUS") {
                    let heap = unsafe { esp_idf_svc::sys::esp_get_free_heap_size() };
                    let uptime_s = unsafe { esp_idf_svc::sys::esp_timer_get_time() } / 1_000_000;
                    let hours = uptime_s / 3600;
                    let mins = (uptime_s % 3600) / 60;
                    respond(&format!("OK heap={}KB up={}h{}m fw=v{}", heap / 1024, hours, mins, FW_VERSION));
                    return;
                }

                // ── CHAT: forward message to Gemini AI via BLE ──
                if let Some(msg) = cmd.strip_prefix("CHAT ").or_else(|| cmd.strip_prefix("chat ")) {
                    let msg = msg.trim().to_string();
                    if msg.is_empty() {
                        respond("ERR empty message");
                        return;
                    }
                    if BLE_CHAT_BUSY.load(Ordering::SeqCst) {
                        respond("ERR busy, please wait");
                        return;
                    }
                    BLE_CHAT_BUSY.store(true, Ordering::SeqCst);

                    let tx_chat = tx.clone();
                    let nvs_chat = nvs_part_inner.clone();
                    std::thread::Builder::new()
                        .name("ble_chat".into())
                        .stack_size(32768)
                        .spawn(move || {
                            let chat_respond = |msg: &str| {
                                tx_chat.lock().set_value(msg.as_bytes());
                                tx_chat.lock().notify();
                            };

                            let nvs = match EspNvs::new(nvs_chat, NVS_NS, true) {
                                Ok(h) => h,
                                Err(e) => {
                                    chat_respond(&format!("ERR NVS: {}", e));
                                    BLE_CHAT_BUSY.store(false, Ordering::SeqCst);
                                    return;
                                }
                            };
                            let api_key = nvs_get(&nvs, "gemini_key").unwrap_or_default();
                            if api_key.is_empty() {
                                chat_respond("ERR no gemini_key configured");
                                BLE_CHAT_BUSY.store(false, Ordering::SeqCst);
                                return;
                            }
                            let model = nvs_get(&nvs, "model_pref")
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| DEFAULT_GEMINI_MODEL.to_string());

                            let url = format!("{}/{}:generateContent?key={}",
                                GEMINI_API, model, api_key);

                            // Escape message for JSON
                            let escaped = msg.replace('\\', "\\\\")
                                .replace('"', "\\\"")
                                .replace('\n', "\\n");

                            let json_body = format!(
                                r#"{{"contents":[{{"role":"user","parts":[{{"text":"{}"}}]}}],"generationConfig":{{"maxOutputTokens":512,"temperature":0.7}}}}"#,
                                escaped
                            );

                            match http_post(&url, &json_body) {
                                Ok(resp_body) => {
                                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&resp_body) {
                                        if let Some(err) = val.get("error") {
                                            let errmsg = err.get("message")
                                                .and_then(|m| m.as_str())
                                                .unwrap_or("API error");
                                            let truncated: String = errmsg.chars().take(120).collect();
                                            chat_respond(&format!("ERR {}", truncated));
                                            BLE_CHAT_BUSY.store(false, Ordering::SeqCst);
                                            return;
                                        }
                                        let text = val.pointer("/candidates/0/content/parts/0/text")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("(empty response)");
                                        // Truncate to fit BLE MTU (~200 chars)
                                        let truncated: String = text.chars().take(200).collect();
                                        if truncated.len() < text.len() {
                                            chat_respond(&format!("OK {}…", truncated));
                                        } else {
                                            chat_respond(&format!("OK {}", text));
                                        }
                                    } else {
                                        chat_respond("ERR parse failed");
                                    }
                                }
                                Err(e) => {
                                    let errmsg: String = e.to_string().chars().take(80).collect();
                                    chat_respond(&format!("ERR {}", errmsg));
                                }
                            }
                            BLE_CHAT_BUSY.store(false, Ordering::SeqCst);
                        })
                        .ok();
                    return;
                }

                respond("ERR unknown cmd");
            });

            // Build advertisement data and start advertising
            let dev_name = ble_device_name();
            let mut adv_data = BLEAdvertisementData::new();
            adv_data.name(&dev_name);
            adv_data.add_service_uuid(uuid128!(NUS_SERVICE_UUID));

            let adv = device.get_advertising();
            {
                let mut adv_lock = adv.lock();
                // Slow advertising: 1–2 sec interval to avoid WiFi radio contention
                adv_lock.min_interval(1600); // 1600 * 0.625ms = 1000ms
                adv_lock.max_interval(3200); // 3200 * 0.625ms = 2000ms
                if let Err(e) = adv_lock.set_data(&mut adv_data) {
                    error!("BLE: set_data failed: {:?}", e);
                }
            }
            match adv.lock().start() {
                Ok(_) => info!("BLE: advertising as '{}' ready", dev_name),
                Err(e) => error!("BLE: advertising start failed: {:?}", e),
            }
}

// ===== 裝置自帶設定門戶 (SoftAP + STA 皆可) =====
// 完全內嵌(手機連設備熱點時沒有網際網路,不能用 CDN)。
// 指令(排程/喇叭/AI 對話)走 POST /api/cmd → 主迴圈 handle_text。生效延遲取決於
// Telegram 長輪詢的剩餘時間:有人正在用控制台時輪詢縮到 1 秒(見 portal_active_recently),
// 所以一般是 1~2 秒;閒置超過一分鐘後的第一個指令最久要等 8 秒。
static PORTAL_HTML: &str = r##"<!doctype html><html lang="zh-Hant"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><title>SerialSoul 控制台</title>
<style>*{box-sizing:border-box;margin:0;font-family:-apple-system,"Microsoft JhengHei",sans-serif}
body{background:#FAFAF9;color:#0C0A09;padding:16px 14px 90px;max-width:520px;margin:0 auto}
h1{font-size:21px;font-weight:700}.sub{color:#78716C;font-size:12px;margin-bottom:14px}
.card{background:#fff;border:1px solid #E7E5E4;border-radius:16px;padding:16px;margin-bottom:12px}
.card h2{font-size:14px;font-weight:700;color:#57534E;margin-bottom:10px;letter-spacing:.5px}
label{display:block;font-size:12px;font-weight:600;color:#57534E;margin:10px 0 4px}
input,select,textarea{width:100%;padding:10px;font-size:15px;border:1px solid #D6D3D1;border-radius:10px;background:#FAFAF9}
input:focus,select:focus,textarea:focus{outline:none;border-color:#CA8A04}
button{padding:12px 14px;font-size:14px;font-weight:600;color:#fff;background:#1C1917;border:none;border-radius:10px;margin-top:10px;cursor:pointer}
button.gold{background:#B7791F}button.sm{padding:6px 10px;font-size:12px;margin:0}
.hint{font-size:11px;color:#A8A29E;margin-top:6px}.g{color:#CA8A04}
.row{display:flex;gap:8px}.row>*{flex:1}
.item{display:flex;justify-content:space-between;align-items:center;padding:8px 10px;border:1px solid #F5F5F4;border-radius:10px;margin-top:6px;font-size:13px;gap:8px}
.item .t{flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis}
.tag{font-size:11px;color:#B7791F;background:#FBF5E9;border-radius:6px;padding:2px 7px;white-space:nowrap}
.tabs{position:fixed;bottom:0;left:0;right:0;display:flex;background:#fff;border-top:1px solid #E7E5E4;max-width:520px;margin:0 auto}
.tabs button{flex:1;background:none;color:#78716C;border-radius:0;margin:0;padding:11px 0;font-size:12px}
.tabs button.on{color:#B7791F;font-weight:700}
.page{display:none}.page.on{display:block}
#cam{width:100%;border-radius:12px;background:#1C1917;min-height:180px;display:block}
#toast{position:fixed;top:12px;left:50%;transform:translateX(-50%);background:#1C1917;color:#fff;padding:10px 18px;border-radius:10px;font-size:13px;display:none;z-index:9}
</style></head><body>
<h1>SerialSoul<span class="g"> 控制台</span></h1><div class="sub" id="devinfo">載入中…</div>
<div id="toast"></div>

<div class="page on" id="p0">
 <div class="card"><h2>📷 即時影像</h2><img id="cam" alt="">
  <div class="row"><button class="gold" onclick="camOn()">開始直播</button><button onclick="camOff()">停止</button></div>
  <div class="hint">直播中裝置較耗電、回覆稍慢；看完請按停止</div></div>
 <div class="card"><h2>💬 跟裝置說話</h2><textarea id="chat" rows="2" placeholder="輸入文字，AI 會在 Telegram 回覆；也可下指令如 /status"></textarea>
  <button class="gold" onclick="sendChat()">送出</button>
  <div class="hint">指令約 1~2 秒生效（閒置一分鐘後第一次可能要 8 秒）</div></div>
 <div class="card"><h2>🔊 喇叭</h2>
  <div class="row"><button onclick="cmd('/audio tone')">播測試音</button></div></div>
</div>

<div class="page" id="p1">
 <div class="card"><h2>⏰ 新增定期任務</h2>
  <label>動作</label><select id="t_act"><option value="watch">👁️ 定時檢查，有事才通知我</option><option value="camera">📷 定時拍照回報（每次都報）</option><option value="gpio">⚡ GPIO 腳位供電</option><option value="alert">🚨 大聲警報+通知</option></select>
  <label>每幾分鐘</label><input id="t_min" type="number" value="30" min="1">
  <div id="t_extra"></div>
  <button class="gold" onclick="addTask()">建立任務</button></div>
 <div class="card" id="roicard" style="display:none"><h2>🖍️ 只看畫面的某一塊（選填）</h2>
  <div class="hint">先按「開始直播」看到畫面，再用手指／滑鼠在畫面上拉一個框，她就只會盯那一塊。</div>
  <canvas id="roi" style="width:100%;border-radius:12px;background:#1C1917;touch-action:none;display:block;margin-top:8px"></canvas>
  <div class="row"><button class="sm" onclick="roiClear()">清除框選</button></div>
  <div id="roitxt" class="hint">尚未框選 — 會看整個畫面</div></div>
 <div class="card"><h2>執行中任務</h2><div id="tasks"><div class="hint">載入中…</div></div></div>
 <div class="card"><h2>每日提醒</h2><div id="rems"><div class="hint">載入中…</div></div>
  <div class="hint">新增提醒：在「跟裝置說話」打「每天早上8點提醒我吃藥」</div></div>
</div>

<div class="page" id="p2">
 <div class="card"><h2>🧠 記憶</h2><div id="mems"></div>
  <div class="hint">新增記憶：跟裝置說「記住 …」</div></div>
 <div class="card"><h2>✨ 技能</h2><div id="skills"></div>
  <details style="margin-top:12px"><summary style="cursor:pointer;font-weight:600">➕ 自訂技能</summary>
   <div class="hint" style="margin:8px 0">教她一件新的事。「名稱」是這個技能叫什麼，「說明」告訴 AI 該做什麼，
    「觸發詞」是你平常會怎麼講。存好之後直接用講的就會通。</div>
   <label>名稱</label><input id="sk_name" placeholder="例：量血壓提醒" maxlength="24">
   <label>說明（告訴 AI 要做什麼）</label><input id="sk_desc" placeholder="例：提醒使用者量血壓並記錄數值" maxlength="80">
   <label>觸發詞（你會怎麼講）</label><input id="sk_trig" placeholder="例：量血壓、血壓多少" maxlength="40">
   <button class="gold" style="width:100%;margin-top:8px" onclick="skAdd()">儲存技能</button>
   <div class="hint" style="margin-top:10px">刪除：輸入名稱後按下面的按鈕</div>
   <button style="width:100%" onclick="skDel()">刪除同名技能</button>
   <div id="sk_msg" class="hint" style="text-align:center"></div>
  </details></div>
</div>

<div class="page" id="p3">
 <div class="card"><h2>📶 WiFi（最多兩組）</h2>
  <label>WiFi 1 名稱</label><input id="wifi_ssid"><label>WiFi 1 密碼</label><input id="wifi_pass" placeholder="不改就留空">
  <label>WiFi 2 名稱</label><input id="wifi_ssid2"><label>WiFi 2 密碼</label><input id="wifi_pass2" placeholder="不改就留空"></div>
 <div class="card"><h2>🔑 Gemini 金鑰</h2>
  <label>Gemini API Key <span id="gs" class="hint"></span></label><input id="gemini_key" placeholder="不改就留空"></div>
 <div class="card"><h2>🔗 Telegram 機器人</h2>
  <label>① 貼上 Bot Token <span id="ts" class="hint"></span></label><input id="tg_token" placeholder="每台裝置要用不同的 bot！">
  <div class="hint">貼好後先按最下面「儲存全部並重啟裝置」</div>
  <label style="margin-top:14px">② 裝置連上網路後，回來這裡按一下綁定</label>
  <div id="tgbind" class="hint">載入中…</div>
  <button class="gold" onclick="bindTelegram()" style="width:100%">點我加好友並綁定</button>
  <div class="hint">按下後 Telegram 會自動開啟你的機器人對話，按 Start 即完成，不需要輸入任何數字 ID。⚠️ 這顆按鈕要「裝置已連上你家 WiFi」才有反應——如果現在還在熱點設定畫面（尚未存 WiFi 重啟），先去上面存好 WiFi 再回來按</div></div>
 <div class="card"><h2>🤖 模型與語音</h2>
  <label>AI 模型</label><select id="model"><option>gemini-flash-latest</option><option>gemini-3.8-flash</option><option>gemini-3.7-flash</option><option>gemini-3.5-flash</option><option>gemini-2.5-flash</option></select>
  <label>語音模式</label><select id="voice"><option value="normal">完整唸出回覆（建議）</option><option value="brief">只唸重點前一兩句</option><option value="off">靜音（螢幕照常顯示）</option></select>
  <label style="margin-top:14px">好聽音色（選填）— TTS 代理網址</label><input id="tts_proxy_url" placeholder="http://192.168.1.x:5050/tts">
  <label>音色名稱</label><input id="tts_proxy_voice" placeholder="zh-TW-HsiaoChenNeural">
  <div class="hint">留空 = 用 Google 免費語音（機械感較重）。想要自然的女生聲音，在家裡電腦跑 <b>tools/tts_proxy.py</b>，把它顯示的網址貼進來。音色可換 zh-TW-HsiaoChenNeural（台灣女聲）／zh-TW-HsiaoYuNeural／zh-CN-XiaoxiaoNeural。</div></div>
 <button class="gold" style="width:100%" onclick="save()">💾 儲存全部並重啟裝置</button>
 <button style="width:100%" onclick="fetch('/api/reboot');toast('重啟中，約 30 秒')">只重啟</button>
 <button style="width:100%" onclick="if(confirm('要換 WiFi 或重新設定嗎？\n\n裝置會重開機並發出熱點，這個網頁會斷線。\n之後用手機連熱點（名稱與 PIN 見裝置螢幕）繼續設定。\n\n現有設定都會保留，按錯了拔電重插就回正常。'))
   {fetch('/api/setup_mode');toast('重開機進入設定模式，請看裝置螢幕')}">📶 換 WiFi／重新設定（進入熱點設定模式）</button>
 <div class="hint">用在「搬家換 WiFi」或「想重新設定」。裝置會重開成熱點，照螢幕上的名稱與 PIN 連進去即可。</div>
 <div id="msg" class="hint" style="text-align:center"></div>
</div>

<div class="tabs">
 <button class="on" onclick="tab(0)">主控</button><button onclick="tab(1)">排程</button>
 <button onclick="tab(2)">記憶</button><button onclick="tab(3)">設定</button>
</div>
<script>
function $(i){return document.getElementById(i)}
function toast(s){const t=$('toast');t.textContent=s;t.style.display='block';setTimeout(()=>t.style.display='none',2600)}
function tab(n){document.querySelectorAll('.page').forEach((p,i)=>p.classList.toggle('on',i==n));document.querySelectorAll('.tabs button').forEach((b,i)=>b.classList.toggle('on',i==n))}
function cmd(c){toast('送出中…');const a=new AbortController();const w=setTimeout(()=>a.abort(),12000);
 return fetch('/api/cmd',{method:'POST',body:JSON.stringify({cmd:c}),signal:a.signal}).then(r=>r.text()).then(t=>{clearTimeout(w);toast(t=='OK'?'已送出':'失敗，請重試');setTimeout(refresh,2500);return t}).catch(()=>{clearTimeout(w);toast('裝置沒回應：請先停止直播再試')})}
function sendChat(){const v=$('chat').value.trim();if(v){cmd(v);$('chat').value=''}}
let camTok='',camRun=false,roiSnap=null;
function camLoop(){if(!camRun)return;const i=$('cam');const n=new Image();
 n.onload=()=>{i.src=n.src;roiSnap=n;const c=$('roi');
  if(c.width!=n.naturalWidth){c.width=n.naturalWidth;c.height=n.naturalHeight}
  roiDraw();setTimeout(camLoop,700)};
 n.onerror=()=>setTimeout(camLoop,2000);
 n.src='/api/frame?token='+camTok+'&t='+Date.now()}
function camOn(){fetch('/api/stream_start').then(r=>r.json()).then(d=>{if(d.ok){camTok=d.path.split('token=')[1];camRun=true;camLoop()}}).catch(()=>toast('啟動失敗'))}
function camOff(){camRun=false;$('cam').src=''}
function extra(){const a=$('t_act').value,e=$('t_extra');
 e.innerHTML=a=='gpio'?'<label>腳位</label><input id="t_pin" type="number" value="21"><label>狀態</label><select id="t_st"><option value="1">供電 HIGH</option><option value="0">斷電 LOW</option></select>'
 :a=='alert'?'<label>警報訊息</label><input id="t_msg" value="警報！請查看">'
 :a=='watch'?'<label>要盯什麼事？用平常講話的方式寫就好</label><textarea id="t_msg" rows="2" placeholder="例：教室裡有沒有人趴著睡覺"></textarea><label style="margin-top:10px"><input type="checkbox" id="t_spk" style="width:auto;display:inline"> 發生時也用喇叭唸出來</label><div class="hint">符合才通知你；不符合完全安靜，不會每次都吵你。</div>'
 :'<label>拍照說明(選填)</label><input id="t_msg" placeholder="定時巡查">';
 $('roicard').style.display=(a=='watch')?'block':'none'}
// 框選:在快照上拉一個矩形,存成畫面百分比。韌體端會翻成「只看這塊區域」的提示詞。
let roiBox=null,roiDrag=null;
function roiPt(ev){const c=$('roi'),r=c.getBoundingClientRect();const t=ev.touches?ev.touches[0]:ev;
 return {x:Math.max(0,Math.min(100,Math.round((t.clientX-r.left)/r.width*100))),
         y:Math.max(0,Math.min(100,Math.round((t.clientY-r.top)/r.height*100)))}}
function roiDraw(){const c=$('roi'),g=c.getContext('2d');g.clearRect(0,0,c.width,c.height);
 if(roiSnap)g.drawImage(roiSnap,0,0,c.width,c.height);
 if(roiBox){g.strokeStyle='#F59E0B';g.lineWidth=3;g.setLineDash([6,4]);
  g.strokeRect(roiBox[0]/100*c.width,roiBox[1]/100*c.height,(roiBox[2]-roiBox[0])/100*c.width,(roiBox[3]-roiBox[1])/100*c.height)}
 $('roitxt').textContent=roiBox?('已框選 '+roiBox[0]+'%,'+roiBox[1]+'% → '+roiBox[2]+'%,'+roiBox[3]+'%'):'尚未框選 — 會看整個畫面'}
function roiClear(){roiBox=null;roiDraw()}
function roiStart(ev){ev.preventDefault();const p=roiPt(ev);roiDrag=p;roiBox=[p.x,p.y,p.x,p.y];roiDraw()}
function roiMove(ev){if(!roiDrag)return;ev.preventDefault();const p=roiPt(ev);
 roiBox=[Math.min(roiDrag.x,p.x),Math.min(roiDrag.y,p.y),Math.max(roiDrag.x,p.x),Math.max(roiDrag.y,p.y)];roiDraw()}
function roiEnd(){roiDrag=null;if(roiBox&&(roiBox[2]-roiBox[0]<5||roiBox[3]-roiBox[1]<5))roiBox=null;roiDraw()}
function addTask(){const a=$('t_act').value,m=$('t_min').value||'30';
 if(a=='watch'){const t=($('t_msg').value||'').trim();
  if(!t){toast('請先寫要盯什麼事');return}
  const roi=roiBox?('[ROI:'+roiBox.join(',')+']'):'';
  cmd('/autotask add watch '+m+' '+roi+t+($('t_spk').checked?' speak':''));return}
 let p=a=='gpio'?($('t_pin').value+':'+$('t_st').value):($('t_msg')?$('t_msg').value:'');
 cmd('/autotask add '+a+' '+m+' '+p)}
function delTask(id){cmd('/tasks del '+id)}
function bindTelegram(){toast('查詢機器人中…');
 const a=new AbortController();const w=setTimeout(()=>a.abort(),8000);
 fetch('/api/telegram_info',{signal:a.signal}).then(r=>r.json()).then(d=>{clearTimeout(w);
  if(d.ok){window.open('https://t.me/'+encodeURIComponent(d.username)+'?start='+encodeURIComponent(d.code),'_blank');toast('請在 Telegram 按 Start 完成綁定')}
  else if(d.error=='no_token')toast('請先貼上面的 Bot Token 並存檔重啟')
  else if(d.error=='offline')toast('裝置還沒連上網路，請先完成 WiFi 設定並重啟裝置')
  else toast('查詢失敗，請稍後再試')
 }).catch(()=>{clearTimeout(w);toast('裝置沒回應，請確認已連上網路')})}
function refresh(){fetch('/api/dump').then(r=>r.json()).then(d=>{
 $('devinfo').textContent='模型 '+(d.model||'?')+' ・ 語音 '+(d.voice||'?')+' ・ 喚醒詞 '+(d.wake||'-');
 if(Array.isArray(d.wifi)){if(d.wifi[0]){$('wifi_ssid').value=d.wifi[0].ssid||''}if(d.wifi[1]){$('wifi_ssid2').value=d.wifi[1].ssid||''}}
 else{if(d.wifi_ssid)$('wifi_ssid').value=d.wifi_ssid;if(d.wifi_ssid2)$('wifi_ssid2').value=d.wifi_ssid2}
 if(d.model)$('model').value=d.model;if(d.voice)$('voice').value=d.voice;
 if(d.tts_url)$('tts_proxy_url').value=d.tts_url;if(d.tts_voice)$('tts_proxy_voice').value=d.tts_voice;
 $('gs').textContent=d.gemini?('· '+(d.gemini===true?'已設定':d.gemini)):'· 未設定';
 $('ts').textContent=d.token?('· '+(d.token===true?'已設定':d.token)):'· 未設定';
 $('tgbind').innerHTML=d.chat_bound?('✅ 已綁定'+(d.bot_username?(' <b>@'+esc(d.bot_username)+'</b>'):'')):'尚未綁定 — 按下面按鈕在 Telegram 加好友即可完成';
 const T={watch:'👁️ 監看',camera:'📷 拍照',gpio:'⚡ GPIO',alert:'🚨 警報'};
 $('tasks').innerHTML=(d.tasks&&d.tasks.length)?d.tasks.map(t=>'<div class="item"><span class="tag">'+(T[t.action]||esc(t.action))+'</span><span class="t">每'+esc(t.interval)+'分 '+esc(t.prompt||'')+'</span><button class="sm" onclick="delTask('+Number(t.id)+')">刪</button></div>').join(''):'<div class="hint">目前沒有任務</div>';
 $('rems').innerHTML=(d.reminders&&d.reminders.length)?d.reminders.map(r=>'<div class="item"><span class="tag">'+esc(r.when)+'</span><span class="t">'+esc(r.msg)+'</span></div>').join(''):'<div class="hint">目前沒有提醒</div>';
 $('mems').innerHTML=(d.memories&&d.memories.length)?d.memories.map(m=>'<div class="item"><span class="t"><b>'+esc(m.k)+'</b>：'+esc(m.v)+'</span></div>').join(''):'<div class="hint">還沒有記憶</div>';
 renderSkills(d.skills);
}).catch(()=>{})}
// 技能瀏覽器:先按分類收起來,點一列才展開看完整內容(含 AI 實際吃到的指示)。
// 使用者不會照著範例講話,所以要讓他看得到「什麼講法會觸發什麼」。
// 提醒、記憶、任務都可能是 AI 依外部文字或圖片寫進去的 —— 一律當不可信字串,
// 進 innerHTML 前必須經過 esc(),否則就是儲存型 XSS(可以替使用者呼叫 /api/*)。
function esc(s){return String(s==null?'':s).replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]))}
function renderSkills(list){const box=$('skills');
 if(!list||!list.length){box.innerHTML='<div class="hint">無</div>';return}
 if(typeof list[0]=='string'){box.innerHTML=list.map(s=>'<span class="tag" style="display:inline-block;margin:3px 4px 0 0">'+esc(s)+'</span>').join('');return}
 const cats=[];list.forEach(s=>{if(cats.indexOf(s.cat)<0)cats.push(s.cat)});
 box.innerHTML=cats.map(c=>'<div style="margin-top:10px"><div class="tag" style="display:inline-block">'+esc(c)+'</div>'
  +list.filter(s=>s.cat==c).map(s=>'<details class="item" style="display:block"><summary style="cursor:pointer;font-weight:600">'
   +esc(s.zh||s.name)+'</summary><div class="hint" style="margin-top:6px"><b>'+esc(s.name)+'</b><br>觸發詞：'
   +esc(s.trigger)+'</div><div class="hint" style="white-space:pre-wrap;margin-top:6px">'+esc(s.detail)+'</div></details>').join('')
  +'</div>').join('')}
// 自訂技能走既有的 /api/cmd 管道送 /skill 指令，跟排程建構器同一套做法，
// 不必為此多開一個端點。名稱/說明/觸發詞用 | 分隔，所以先擋掉使用者輸入的 |。
function skAdd(){const n=$('sk_name').value.trim(),d=$('sk_desc').value.trim(),t=$('sk_trig').value.trim();
 if(!n||!d||!t){$('sk_msg').textContent='三個欄位都要填';return}
 if((n+d+t).indexOf('|')>=0){$('sk_msg').textContent='內容不能包含「|」符號';return}
 $('sk_msg').textContent='儲存中…';
 cmd('/skill add '+n+'|'+d+'|'+t);
 setTimeout(()=>{$('sk_msg').textContent='✓ 已送出，稍後重新整理即可看到';refresh()},2500)}
function skDel(){const n=$('sk_name').value.trim();
 if(!n){$('sk_msg').textContent='請先在「名稱」填入要刪除的技能';return}
 if(!confirm('確定刪除技能「'+n+'」？'))return;
 $('sk_msg').textContent='刪除中…';
 cmd('/skill del '+n);
 setTimeout(()=>{$('sk_msg').textContent='✓ 已送出';refresh()},2500)}
function save(){
 const b={};for(const k of ['wifi_ssid','wifi_pass','wifi_ssid2','wifi_pass2','gemini_key','tg_token','tts_proxy_url','tts_proxy_voice']){const v=$(k).value.trim();if(v)b[k]=v}
 b.model_pref=$('model').value;b.voice_mode=$('voice').value;
 $('msg').textContent='儲存中…';camOff();
 const a=new AbortController();const w=setTimeout(()=>a.abort(),15000);
 fetch('/api/save',{method:'POST',body:JSON.stringify(b),signal:a.signal}).then(r=>r.text()).then(t=>{clearTimeout(w);
  if(t.indexOf('OK')>=0){$('msg').textContent='✓ 已儲存，重啟中（約 30 秒）';setTimeout(()=>fetch('/api/reboot'),800)}
  else $('msg').textContent='儲存失敗，請重試'}).catch(()=>{clearTimeout(w);$('msg').textContent='裝置沒回應：請先停止直播、10 秒後再按一次'})}
$('t_act').onchange=extra;extra();
{const c=$('roi');c.addEventListener('mousedown',roiStart);c.addEventListener('mousemove',roiMove);
 window.addEventListener('mouseup',roiEnd);c.addEventListener('touchstart',roiStart,{passive:false});
 c.addEventListener('touchmove',roiMove,{passive:false});c.addEventListener('touchend',roiEnd)}
refresh();setInterval(refresh,20000);
</script></body></html>"##;

/// 完整狀態 JSON(/dump 與門戶 /api/dump 共用)。
/// mask_wifi_pass=false → USB 序列 /dump(等同實體接觸,密碼明文方便設定工具回填);
/// mask_wifi_pass=true  → 網頁門戶快照(密碼一律 **** — UI 用「不改就留空」不需要明文)。
/// API 金鑰兩者都遮罩前4後4。
fn build_dump_json(state: &AppState, nvs: &EspNvs<NvsDefault>, mask_wifi_pass: bool) -> serde_json::Value {
    let g = |k: &str| nvs_get(nvs, k).unwrap_or_default();
    let mask = |k: &str| {
        let v = nvs_get(nvs, k).unwrap_or_default();
        if v.len() > 8 { format!("{}...{}", str_prefix(&v, 4), str_suffix(&v, 4)) }
        else if v.is_empty() { String::new() } else { "****".to_string() }
    };
    let wifi: Vec<serde_json::Value> = (1..=5u8).map(|i| {
        let (sk, pk) = if i == 1 { ("wifi_ssid".to_string(), "wifi_pass".to_string()) }
                       else { (format!("wifi_ssid{}", i), format!("wifi_pass{}", i)) };
        let pass = g(&pk);
        let pass_out = if mask_wifi_pass && !pass.is_empty() { "****".to_string() } else { pass };
        serde_json::json!({ "ssid": g(&sk), "pass": pass_out })
    }).collect();
    // 控制台的技能瀏覽器要看到完整內容(分類/白話說明/AI 實際吃的指示),
    // 不是只有一排名字 —— 使用者要能點進去確認「這台到底會做什麼」。
    let skills: Vec<serde_json::Value> = state.skills.iter().map(|s| {
        let (cat, zh) = skill_meta(&s.name);
        serde_json::json!({ "name": s.name, "cat": cat, "zh": zh,
                            "trigger": s.trigger, "detail": s.description })
    }).collect();
    let mems: Vec<serde_json::Value> = state.memories.iter()
        .map(|(k, v)| serde_json::json!({ "k": k, "v": v })).collect();
    let rems: Vec<serde_json::Value> = state.reminders.iter().filter(|r| r.active).map(|r| {
        let when = match r.rtype {
            ReminderType::Interval => format!("每{}分", r.interval_secs / 60),
            _ => format!("每天{:02}:{:02}", r.hour, r.minute),
        };
        serde_json::json!({ "id": r.id, "when": when, "msg": r.message })
    }).collect();
    let tasks: Vec<serde_json::Value> = state.auto_tasks.iter().filter(|t| t.active).map(|t|
        serde_json::json!({ "id": t.id, "interval": t.interval_secs / 60, "action": t.action, "prompt": t.prompt, "speak": t.speak })
    ).collect();
    serde_json::json!({
        "wifi": wifi, "model": state.current_model, "voice": state.voice_mode, "wake": state.wake_phrase,
        "gemini": mask("gemini_key"), "token": mask("tg_token"), "chatid": g("chat_id"),
        "chat_bound": !g("chat_id").is_empty(), "bot_username": g("tg_bot_username"),
        "tts_url": g(TTS_PROXY_URL_KEY), "tts_voice": g(TTS_PROXY_VOICE_KEY),
        "skills": skills, "memories": mems, "reminders": rems, "tasks": tasks
    })
}

/// 主迴圈每輪呼叫:刷新門戶 /api/dump 要回的狀態快照(WiFi 密碼遮罩版)。
fn refresh_portal_state_json(state: &AppState, nvs: &EspNvs<NvsDefault>) {
    let json = build_dump_json(state, nvs, true).to_string();
    *PORTAL_STATE_JSON.lock().unwrap_or_else(|e| e.into_inner()) = json;
}

/// 讀 NVS 組成門戶用的設定 JSON(金鑰只回報有無,不外洩)。
fn portal_config_json(nvs_part: &EspDefaultNvsPartition) -> String {
    let nvs = match EspNvs::new(nvs_part.clone(), NVS_NS, true) {
        Ok(n) => n,
        Err(_) => return "{}".to_string(),
    };
    let g = |k: &str| nvs_get(&nvs, k).unwrap_or_default().replace('"', "'");
    let setf = |k: &str| nvs_get(&nvs, k).filter(|s| !s.is_empty()).is_some();
    format!(
        r#"{{"wifi_ssid":"{}","wifi_ssid2":"{}","model":"{}","voice":"{}","wake":"{}","chat_id":"{}","chat_bound":{},"bot_username":"{}","gemini":{},"token":{}}}"#,
        g("wifi_ssid"), g("wifi_ssid2"), g("model_pref"), g("voice_mode"), g("wake_phrase"), g("chat_id"),
        !g("chat_id").is_empty(), g("tg_bot_username"),
        setf("gemini_key"), setf("tg_token")
    )
}

// 登入頁(與控制台同風格,自帶無 CDN)。PIN = 開機 LCD/日誌顯示的 6 位配對 PIN。
static PORTAL_LOGIN_HTML: &str = r##"<!doctype html><html lang="zh-Hant"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><title>SerialSoul 登入</title>
<style>*{box-sizing:border-box;margin:0;font-family:-apple-system,"Microsoft JhengHei",sans-serif}
body{background:#FAFAF9;color:#0C0A09;display:flex;align-items:center;justify-content:center;min-height:100vh;padding:20px}
.card{background:#fff;border:1px solid #E7E5E4;border-radius:20px;padding:32px 26px;width:100%;max-width:360px;text-align:center}
h1{font-size:22px;font-weight:700}.g{color:#CA8A04}.sub{color:#78716C;font-size:13px;margin:8px 0 22px}
input{width:100%;padding:14px;font-size:22px;text-align:center;letter-spacing:8px;border:1px solid #D6D3D1;border-radius:12px;background:#FAFAF9}
input:focus{outline:none;border-color:#CA8A04}
button{width:100%;padding:14px;font-size:16px;font-weight:600;color:#fff;background:#B7791F;border:none;border-radius:12px;margin-top:16px;cursor:pointer}
#err{color:#B91C1C;font-size:13px;margin-top:12px;min-height:18px}</style></head>
<body><div class="card"><h1>SerialSoul<span class="g"> 登入</span></h1>
<div class="sub">輸入裝置的 6 位 PIN 碼<br>(開機時顯示在裝置螢幕上)</div>
<input id="pin" inputmode="numeric" maxlength="8" placeholder="······" autofocus>
<button onclick="login()">進入控制台</button><div id="err"></div>
<script>
const p=document.getElementById('pin');p.addEventListener('keydown',e=>{if(e.key=='Enter')login()});
function login(){const v=p.value.trim();if(!v)return;
 fetch('/api/login',{method:'POST',body:JSON.stringify({pin:v})}).then(r=>r.text().then(t=>({s:r.status,t})))
 .then(({s,t})=>{if(s==200&&t=='OK'){location.reload()}
 else if(s==429)document.getElementById('err').textContent='錯誤次數過多，請 5 分鐘後再試';
 else{document.getElementById('err').textContent='PIN 錯誤，請再試一次';p.value='';p.focus()}})
 .catch(()=>document.getElementById('err').textContent='連線失敗，請重試');}
</script></div></body></html>"##;

/// 驗證 Cookie 裡的 session token(sid=…)是否為有效登入。
fn portal_session_ok(cookie: Option<&str>) -> bool {
    let Some(c) = cookie else { return false };
    let Some(pos) = c.find("sid=") else { return false };
    let token: String = c[pos + 4..].chars().take_while(|ch| ch.is_ascii_hexdigit()).collect();
    if token.len() < 16 { return false; }
    PORTAL_SESSIONS.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|t| t == &token)
}

/// 把門戶送來的 JSON 設定寫入 NVS。回傳是否成功。
fn portal_save_config(nvs_part: &EspDefaultNvsPartition, body: &str) -> bool {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let mut nvs = match EspNvs::new(nvs_part.clone(), NVS_NS, true) {
        Ok(n) => n,
        Err(_) => return false,
    };
    for key in ["wifi_ssid", "wifi_pass", "wifi_ssid2", "wifi_pass2",
                "gemini_key", "tg_token", "chat_id", "model_pref", "wake_phrase",
                TTS_PROXY_URL_KEY, TTS_PROXY_VOICE_KEY] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            if !s.is_empty() { let _ = nvs.set_str(key, s); }
        }
    }
    // voice_mode 先正規化再存 —— 直接寫原字串的話,網頁送來一個韌體不認得的值
    // (例如舊版下拉選單的 "on")就會被原樣存進 NVS,之後 should_speak_onboard
    // 一律回 false,裝置變成靜音而且完全沒有錯誤訊息可查。
    if let Some(s) = v.get("voice_mode").and_then(|x| x.as_str()) {
        if let Some(normalized) = normalize_voice_mode(s) {
            let _ = nvs.set_str("voice_mode", normalized);
        }
    }
    true
}

/// 在裝置上啟動 HTTP 設定門戶(SoftAP 的 192.168.4.1 或連上 WiFi 後的區網 IP 都適用)。
/// 回傳的 server 必須保持存活(drop 會停止伺服器)。
fn start_config_portal(nvs_part: EspDefaultNvsPartition)
    -> anyhow::Result<esp_idf_svc::http::server::EspHttpServer<'static>>
{
    use esp_idf_svc::http::server::{EspHttpServer, Configuration as HttpSrvConfig};
    use esp_idf_svc::http::Method;
    use esp_idf_svc::io::Write as _;

    // 串流 handler 直接跑在 httpd task 上,預設 6KB 堆疊不夠(拍照+格式化),加到 10KB。
    // max_open_sockets 4→7:Chrome 一個分頁就開多條 keep-alive 連線,再加 MJPEG
    // 串流長佔一條,4 條會全滿 → 之後所有請求(含 POST 儲存/指令)一律卡死。
    let mut server = EspHttpServer::new(&HttpSrvConfig {
        stack_size: 10240,
        max_open_sockets: 7,
        lru_purge_enable: true,
        uri_match_wildcard: true, // 允許註冊 /* 萬用路徑(捕捉手機的 captive portal 偵測)
        ..Default::default()
    })?;

    // 未登入 → 給登入頁;已登入 → 控制台。PIN=開機顯示的 6 位配對 PIN。
    server.fn_handler("/", Method::Get, |req| {
        if portal_session_ok(req.header("Cookie")) {
            req.into_ok_response()?.write_all(PORTAL_HTML.as_bytes())
        } else {
            req.into_ok_response()?.write_all(PORTAL_LOGIN_HTML.as_bytes())
        }
    })?;

    // PIN 登入:發 session token(Cookie 存 30 天)。連錯 5 次鎖 5 分鐘(防暴力破解)。
    let np_login = nvs_part.clone();
    server.fn_handler("/api/login", Method::Post, move |mut req| {
        let now = now_epoch().max(0) as u32;
        if now < PORTAL_LOCK_UNTIL.load(Ordering::Relaxed) {
            return req.into_response(429, Some("Locked"), &[])?.write_all(b"LOCKED");
        }
        // 同 /api/save:body 被分段就會讀到半截,登入會莫名其妙失敗。
        let mut buf = vec![0u8; 256];
        let want = req.header("Content-Length")
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0)
            .min(buf.len());
        let mut n = 0usize;
        if want > 0 {
            while n < want {
                match req.read(&mut buf[n..want]) {
                    Ok(0) => break,
                    Ok(k) => n += k,
                    Err(_) => break,
                }
            }
        } else {
            n = req.read(&mut buf).unwrap_or(0);
        }
        let body = String::from_utf8_lossy(&buf[..n]).to_string();
        let pin = serde_json::from_str::<serde_json::Value>(&body).ok()
            .and_then(|v| v.get("pin").and_then(|p| p.as_str()).map(|s| s.to_string()))
            .unwrap_or_default();
        let expected = match EspNvs::new(np_login.clone(), NVS_NS, true) {
            Ok(nvs) => nvs_get(&nvs, "ble_pin").unwrap_or_default(),
            Err(_) => String::new(),
        };
        if expected.is_empty() || pin != expected {
            let fails = PORTAL_AUTH_FAILS.fetch_add(1, Ordering::Relaxed) + 1;
            if fails >= 5 {
                PORTAL_LOCK_UNTIL.store(now + 300, Ordering::Relaxed);
                PORTAL_AUTH_FAILS.store(0, Ordering::Relaxed);
                warn!("門戶 PIN 連錯 5 次,鎖定 5 分鐘");
            }
            return req.into_response(403, Some("Forbidden"), &[])?.write_all(b"BAD_PIN");
        }
        PORTAL_AUTH_FAILS.store(0, Ordering::Relaxed);
        let token = format!("{:08x}{:08x}",
            unsafe { esp_idf_svc::sys::esp_random() },
            unsafe { esp_idf_svc::sys::esp_random() });
        {
            let mut s = PORTAL_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
            if s.len() >= 8 { s.remove(0); } // 最多 8 個 session(多裝置/家人),舊的先淘汰
            s.push(token.clone());
        }
        info!("門戶登入成功,發放 session");
        // 剛登入 = 準備要操作了 → 先進快模式,第一個動作就不用等長輪詢。
        // 只在「真的有動作」時標記(登入/送指令),不含每 20 秒的 /api/dump 自動刷新 ——
        // 否則一個忘了關的分頁會讓裝置永遠用 8 倍的 Telegram API 呼叫量。
        mark_portal_activity();
        // Cookie 由伺服器發、加 HttpOnly:頁面上的 JS(包括萬一被注入的)讀不到 session。
        let cookie = format!("sid={}; Max-Age=2592000; Path=/; SameSite=Strict; HttpOnly", token);
        req.into_response(200, Some("OK"), &[("Set-Cookie", cookie.as_str())])?.write_all(b"OK")
    })?;

    let np_dump = nvs_part.clone();
    server.fn_handler("/api/dump", Method::Get, move |req| {
        if !portal_session_ok(req.header("Cookie")) {
            return req.into_response(401, Some("Unauthorized"), &[])?.write_all(b"AUTH");
        }
        // 主迴圈跑起來後有完整快照(排程/記憶/技能全都有);開機初期/配網模式退回 NVS 版
        let snapshot = PORTAL_STATE_JSON.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let json = if snapshot.is_empty() { portal_config_json(&np_dump) } else { snapshot };
        req.into_ok_response()?.write_all(json.as_bytes())
    })?;

    // Telegram 一鍵綁定:查詢已存的 bot token 對應的機器人帳號,
    // 讓控制台顯示「點我加好友」連結 — 使用者按 Start 後裝置自動學會 chat_id,
    // 不需要再去問 @userinfobot 抄一串數字。此呼叫需要裝置已連上網際網路
    // (SoftAP 配網當下沒有網路,只有連上家用 WiFi 後才查得到)。
    let np_tg = nvs_part.clone();
    server.fn_handler("/api/telegram_info", Method::Get, move |req| {
        if !portal_session_ok(req.header("Cookie")) {
            return req.into_response(401, Some("Unauthorized"), &[])?.write_all(b"AUTH");
        }
        let token = match EspNvs::new(np_tg.clone(), NVS_NS, true) {
            Ok(nvs) => nvs_get(&nvs, "tg_token").unwrap_or_default(),
            Err(_) => String::new(),
        };
        if token.is_empty() {
            return req.into_ok_response()?.write_all(br#"{"ok":false,"error":"no_token"}"#);
        }
        // 先快速探測有沒有網路(3 秒逾時) 再考慮呼叫 Telegram getMe(可能卡到 30 秒)。
        // esp httpd 是單執行緒:沒這個短路,SoftAP 配網階段(必然離線)使用者按下
        // 這顆按鈕會讓整個門戶凍住最多 30 秒,看起來像「點下去沒反應」。
        if !internet_reachable() {
            return req.into_ok_response()?.write_all(br#"{"ok":false,"error":"offline"}"#);
        }
        let resp = match fetch_bot_username(&token) {
            Ok(username) => {
                if let Ok(mut nvs) = EspNvs::new(np_tg.clone(), NVS_NS, true) {
                    let _ = nvs.set_str("tg_bot_username", &username);
                }
                format!(r#"{{"ok":true,"username":"{}","code":"{}"}}"#, username, tg_bind_code())
            }
            Err(e) => {
                warn!("telegram_info 查詢失敗(可能尚未連網): {}", e);
                r#"{"ok":false,"error":"fetch_failed"}"#.to_string()
            }
        };
        req.into_ok_response()?.write_all(resp.as_bytes())
    })?;

    // 網頁指令通道:排程/喇叭/模型/AI 對話…全部指令與 Telegram 同一套引擎
    server.fn_handler("/api/cmd", Method::Post, |mut req| {
        if !portal_session_ok(req.header("Cookie")) {
            return req.into_response(401, Some("Unauthorized"), &[])?.write_all(b"AUTH");
        }
        // 512 對「網頁送 AI 對話」太小,長一點的句子會被截掉;一併加大並讀滿。
        // 放堆積不放堆疊:httpd task 的堆疊只有 10240 bytes。
        let mut buf = vec![0u8; 4096];
        let want = req.header("Content-Length")
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0)
            .min(buf.len());
        let mut n = 0usize;
        if want > 0 {
            while n < want {
                match req.read(&mut buf[n..want]) {
                    Ok(0) => break,
                    Ok(k) => n += k,
                    Err(_) => break,
                }
            }
        } else {
            n = req.read(&mut buf).unwrap_or(0);
        }
        let body = String::from_utf8_lossy(&buf[..n]).to_string();
        let cmd = serde_json::from_str::<serde_json::Value>(&body).ok()
            .and_then(|v| v.get("cmd").and_then(|c| c.as_str()).map(|s| s.to_string()))
            .unwrap_or(body);
        let cmd = cmd.trim().to_string();
        if cmd.is_empty() || cmd.len() > 400 {
            return req.into_ok_response()?.write_all(b"ERR");
        }
        PORTAL_CMDS.lock().unwrap_or_else(|e| e.into_inner()).push(cmd);
        mark_portal_activity();
        req.into_ok_response()?.write_all(b"OK")
    })?;

    let np_save = nvs_part.clone();
    server.fn_handler("/api/save", Method::Post, move |mut req| {
        if !portal_session_ok(req.header("Cookie")) {
            return req.into_response(401, Some("Unauthorized"), &[])?.write_all(b"AUTH");
        }
        // 單次 read() 只保證讀到「當下可讀的」位元組。body 被 TCP 分成兩段
        // (網路抖動時很常見)就會拿到半截 JSON,解析失敗只回一句 ERR ——
        // 使用者看到的是「WiFi 設定有時候存不進去」。這裡要讀滿 Content-Length。
        // buffer 放堆積不放堆疊:httpd task 的堆疊只有 10240 bytes。
        let mut buf = vec![0u8; 4096];
        let want = req.header("Content-Length")
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0)
            .min(buf.len());
        let mut n = 0usize;
        if want > 0 {
            while n < want {
                match req.read(&mut buf[n..want]) {
                    Ok(0) => break,
                    Ok(k) => n += k,
                    Err(_) => break,
                }
            }
        } else {
            n = req.read(&mut buf).unwrap_or(0);
        }
        let body = String::from_utf8_lossy(&buf[..n]).to_string();
        let ok = portal_save_config(&np_save, &body);
        req.into_ok_response()?.write_all(if ok { b"OK" } else { b"ERR" })
    })?;

    // 即時影像改走 80 埠 httpd 本身(multipart MJPEG):
    // 獨立 8080 TcpListener 在此板上外部連線一律被 RST(lwIP/省電交互問題),
    // 走同一個 httpd 則與門戶同源同埠,AP 模式(192.168.4.1)也直接可用。
    server.fn_handler("/api/stream_start", Method::Get, move |req| {
        if !portal_session_ok(req.header("Cookie")) {
            return req.into_response(401, Some("Unauthorized"), &[])?.write_all(b"AUTH");
        }
        let token = {
            let mut t = PORTAL_STREAM_TOKEN.lock().unwrap_or_else(|e| e.into_inner());
            if t.is_empty() {
                *t = format!("{:08x}", unsafe { esp_idf_svc::sys::esp_random() });
            }
            t.clone()
        };
        let resp = format!(r#"{{"ok":true,"path":"/api/stream?token={}"}}"#, token);
        req.into_ok_response()?.write_all(resp.as_bytes())
    })?;

    // 單張快照:網頁輪詢這個端點做「準即時」畫面(~1fps)。
    // 不能用 multipart 串流當主要方案:esp httpd 單執行緒,無限迴圈 handler
    // 會讓門戶其他請求(儲存/指令)全部卡死等到串流結束。
    let np_frame = nvs_part.clone();
    server.fn_handler("/api/frame", Method::Get, move |req| {
        // 補上 session 驗證(先前只查串流 token,PIN 沒登入也能偷看鏡頭 — 審查抓到的漏洞)。
        // <img>/Image() 同源請求會自動帶 Cookie,前端不用改。
        if !portal_session_ok(req.header("Cookie")) {
            return req.into_response(401, Some("Unauthorized"), &[])?.write_all(b"AUTH");
        }
        let uri = req.uri().to_string();
        let expected = PORTAL_STREAM_TOKEN.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if expected.is_empty() || !uri.contains(&format!("token={}", expected)) {
            req.into_response(403, Some("Forbidden"), &[])?.write_all(b"bad token")?;
            return Ok(());
        }
        let pins = match EspNvs::new(np_frame.clone(), NVS_NS, true) {
            Ok(nvs) => load_camera_pins(&nvs),
            Err(_) => {
                req.into_response(500, Some("ERR"), &[])?.write_all(b"nvs")?;
                return Ok(());
            }
        };
        match capture_camera_jpeg(&pins) {
            Ok(jpeg) => {
                let mut resp = req.into_response(200, Some("OK"), &[
                    ("Content-Type", "image/jpeg"),
                    ("Cache-Control", "no-cache, no-store, must-revalidate"),
                ])?;
                resp.write_all(&jpeg)?;
            }
            Err(e) => {
                warn!("portal frame capture error: {}", e);
                req.into_response(500, Some("ERR"), &[])?.write_all(b"cam")?;
            }
        }
        Ok::<(), esp_idf_svc::io::EspIOError>(())
    })?;

    let np_stream = nvs_part.clone();
    server.fn_handler("/api/stream", Method::Get, move |req| {
        // 同上補 session 驗證(進階 VLC/瀏覽器直連用途仍可用,但要先登入拿過 Cookie)。
        if !portal_session_ok(req.header("Cookie")) {
            return req.into_response(401, Some("Unauthorized"), &[])?.write_all(b"AUTH");
        }
        let uri = req.uri().to_string();
        let expected = PORTAL_STREAM_TOKEN.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if expected.is_empty() || !uri.contains(&format!("token={}", expected)) {
            req.into_response(403, Some("Forbidden"), &[])?.write_all(b"bad token")?;
            return Ok(());
        }
        let pins = match EspNvs::new(np_stream.clone(), NVS_NS, true) {
            Ok(nvs) => load_camera_pins(&nvs),
            Err(_) => {
                req.into_response(500, Some("ERR"), &[])?.write_all(b"nvs")?;
                return Ok(());
            }
        };
        let mut resp = req.into_response(200, Some("OK"), &[
            ("Content-Type", "multipart/x-mixed-replace; boundary=jpgframe"),
            ("Cache-Control", "no-cache, no-store, must-revalidate"),
        ])?;
        // ⚠️ 這個 multipart 串流會獨佔 httpd task,期間門戶其他請求全部排隊!
        // 只給進階用途(VLC 等單一觀看),網頁 UI 一律用 /api/frame 輪詢。
        // 上限 15 秒自動斷線,把門戶還回來(評估報告 P1-4)。
        let stream_deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut consecutive_fails = 0u32;
        while std::time::Instant::now() < stream_deadline {
            match capture_camera_jpeg(&pins) {
                Ok(jpeg) => {
                    consecutive_fails = 0;
                    let hdr = format!(
                        "--jpgframe\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                        jpeg.len()
                    );
                    if resp.write_all(hdr.as_bytes()).is_err() { break; }
                    if resp.write_all(&jpeg).is_err() { break; }
                    if resp.write_all(b"\r\n").is_err() { break; }
                }
                Err(e) => {
                    warn!("portal stream capture error: {}", e);
                    consecutive_fails += 1;
                    if consecutive_fails >= 5 { break; }
                    std::thread::sleep(Duration::from_millis(400));
                }
            }
            std::thread::sleep(Duration::from_millis(200)); // ~5 fps,夠看且省 RAM/頻寬
        }
        Ok::<(), esp_idf_svc::io::EspIOError>(())
    })?;

    server.fn_handler("/api/reboot", Method::Get, |req| {
        if !portal_session_ok(req.header("Cookie")) {
            return req.into_response(401, Some("Unauthorized"), &[])?.write_all(b"AUTH");
        }
        let _ = req.into_ok_response()?.write_all(b"rebooting");
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(500));
            unsafe { esp_idf_svc::sys::esp_restart(); }
        });
        Ok::<(), esp_idf_svc::io::EspIOError>(())
    })?;

    // 已連上 WiFi 時要換網路/改設定,得先把裝置弄回熱點模式。設一次性旗標再重開,
    // 不動既有設定 —— 使用者按錯或改變心意,拔電重插就回正常運作。
    {
        let np = nvs_part.clone();
        server.fn_handler("/api/setup_mode", Method::Get, move |req| {
            if !portal_session_ok(req.header("Cookie")) {
                return req.into_response(401, Some("Unauthorized"), &[])?.write_all(b"AUTH");
            }
            if let Ok(mut nvs) = EspNvs::new(np.clone(), NVS_NS, true) {
                let _ = nvs.set_str(FORCE_SETUP_KEY, "1");
            }
            let _ = req.into_ok_response()?.write_all(b"ok");
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(500));
                unsafe { esp_idf_svc::sys::esp_restart(); }
            });
            Ok::<(), esp_idf_svc::io::EspIOError>(())
        })?;
    }

    // Captive portal 彈窗:手機連上熱點會偵測網路(Android 打 generate_204、
    // iOS 打 hotspot-detect.html、Windows 打 connecttest.txt)。captive DNS 已把
    // 這些網域劫持到本機,這裡再把「任何未知路徑」302 到首頁(相對路徑,STA/AP 通用)
    // → 手機自動跳出「需要登入」→ 點開就是設定頁,使用者完全不用手打網址。
    // 註:精確路徑(/、/api/*)先註冊先匹配,不受此萬用路徑影響。
    server.fn_handler("/*", Method::Get, |req| {
        req.into_response(302, Some("Found"), &[("Location", "/")])?
            .write_all(b"redirect")
    })?;

    Ok(server)
}

/// 極簡 DNS 劫持:熱點模式下所有 DNS 查詢都答 192.168.4.1,
/// 手機連上熱點後系統偵測到 captive portal 會自動跳出設定頁。
fn start_captive_dns(ap_ip: [u8; 4]) {
    let _ = std::thread::Builder::new()
        .name("captive_dns".into())
        .stack_size(4096)
        .spawn(move || {
            let socket = match std::net::UdpSocket::bind("0.0.0.0:53") {
                Ok(s) => s,
                Err(e) => { warn!("captive DNS bind 失敗: {:?}", e); return; }
            };
            let mut buf = [0u8; 512];
            loop {
                let (len, src) = match socket.recv_from(&mut buf) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if len < 12 { continue; }
                let mut resp = buf[..len].to_vec();
                resp[2] = 0x84; // QR=response, AA=1
                resp[3] = 0x00;
                resp[6] = 0; resp[7] = 1;                     // ANCOUNT = 1
                resp[8] = 0; resp[9] = 0; resp[10] = 0; resp[11] = 0;
                // Answer: name 指回 offset 12, A record, TTL 60, 指向 AP 實際 IP
                // (不能寫死 192.168.4.1 — 此板 AP netif 實際發 192.168.71.1,實測抓到的 bug)
                resp.extend_from_slice(&[0xC0, 0x0C, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4,
                                         ap_ip[0], ap_ip[1], ap_ip[2], ap_ip[3]]);
                let _ = socket.send_to(&resp, src);
            }
        });
}

/// SoftAP 配網模式:發開放熱點 → 手機連上 → captive portal 自動跳設定頁。
/// 使用者存檔後裝置重啟,改走正常 WiFi 流程。此函式不返回。
fn run_softap_portal_mode(wifi: &mut EspWifi<'static>, nvs_part: EspDefaultNvsPartition, reason: &str) -> ! {
    let ap_ssid = ble_device_name(); // "SerialSoul-XXXX"
    warn!("=== SoftAP 配網模式 ({}) === 熱點: {} → 手機連上後開設定頁(IP 見下方日誌/LCD)", reason, ap_ssid);

    if wifi.is_started().unwrap_or(false) { let _ = wifi.stop(); }
    let mut ap_cfg = AccessPointConfiguration::default();
    if let Ok(s) = ap_ssid.as_str().try_into() { ap_cfg.ssid = s; }
    // 加密熱點:每次進來都換一組新密碼(不寫 NVS —— 24KB 已經很擠,而且一次性更安全)。
    // 上面已確定 WiFi 是停止狀態,random_ap_pass() 的熵源前提成立。
    let ap_pass: Option<String> = if SETUP_AP_SECURED { Some(random_ap_pass()) } else { None };
    match ap_pass.as_deref() {
        Some(p) => {
            // 加密模式下絕不退回開放熱點:轉換失敗就留空密碼,set_configuration 會拒絕而不是裸奔。
            ap_cfg.auth_method = esp_idf_svc::wifi::AuthMethod::WPA2Personal;
            match p.try_into() {
                Ok(pw) => ap_cfg.password = pw,
                Err(_) => error!("熱點密碼轉換失敗,熱點不會以開放模式啟動"),
            }
        }
        None => {
            ap_cfg.auth_method = esp_idf_svc::wifi::AuthMethod::None; // 開放熱點,零門檻配網
        }
    }
    ap_cfg.max_connections = 4;
    if let Err(e) = wifi.set_configuration(&Configuration::AccessPoint(ap_cfg)) {
        warn!("SoftAP set_configuration 失敗: {:?}", e);
    }
    if let Err(e) = wifi.start() {
        warn!("SoftAP start 失敗: {:?}", e);
    }

    // 1) 把 AP IP 改成好記的 10.1.1.1(原 192.168.71.1 太長,使用者難打)。
    // 2) DHCP 同時發「DNS 伺服器=本機」:預設 dhcps 不發 DNS(實測 ipconfig
    //    無 DNS Servers 欄位) → 手機解析不了偵測網域 → 不會自動彈設定頁。
    unsafe {
        use esp_idf_svc::sys as esys;
        use esp_idf_svc::handle::RawHandle as _;
        let netif = wifi.ap_netif().handle();
        esys::esp_netif_dhcps_stop(netif);
        let ip_info = esys::esp_netif_ip_info_t {
            ip: esys::esp_ip4_addr_t { addr: u32::from_le_bytes([10, 1, 1, 1]) },
            netmask: esys::esp_ip4_addr_t { addr: u32::from_le_bytes([255, 255, 255, 0]) },
            gw: esys::esp_ip4_addr_t { addr: u32::from_le_bytes([10, 1, 1, 1]) },
        };
        if esys::esp_netif_set_ip_info(netif, &ip_info) != 0 {
            warn!("SoftAP 自訂 IP 失敗,沿用預設");
        }
        let mut dns = esys::esp_netif_dns_info_t::default();
        dns.ip.u_addr.ip4.addr = u32::from_le_bytes([10, 1, 1, 1]);
        dns.ip.type_ = 0; // IPADDR_TYPE_V4
        esys::esp_netif_set_dns_info(netif, esys::esp_netif_dns_type_t_ESP_NETIF_DNS_MAIN, &mut dns);
        let mut offer_dns: u8 = 1; // dhcps OFFER_DNS
        esys::esp_netif_dhcps_option(
            netif,
            esys::esp_netif_dhcp_option_mode_t_ESP_NETIF_OP_SET,
            esys::esp_netif_dhcp_option_id_t_ESP_NETIF_DOMAIN_NAME_SERVER,
            &mut offer_dns as *mut u8 as *mut core::ffi::c_void,
            1,
        );
        esys::esp_netif_dhcps_start(netif);
    }

    // 讀 AP netif「實際」IP(自訂成功=10.1.1.1;失敗則讀到板子預設值),
    // captive DNS 與 LCD 指引都用真實 IP,不寫死。
    let ap_ip: [u8; 4] = wifi.ap_netif().get_ip_info()
        .map(|i| i.ip.octets())
        .unwrap_or([10, 1, 1, 1]);
    let ap_ip_str = format!("{}.{}.{}.{}", ap_ip[0], ap_ip[1], ap_ip[2], ap_ip[3]);
    info!("SoftAP IP: {}", ap_ip_str);

    start_captive_dns(ap_ip);
    let _server = match start_config_portal(nvs_part.clone()) {
        Ok(s) => Some(s),
        Err(e) => { warn!("設定門戶啟動失敗: {:?}", e); None }
    };

    // LCD 顯示配網指引 + 登入 PIN,開箱不用接電腦。
    // 使用者手上只有手機、沒有說明書,所以熱點名稱/網址/PIN 三件事要一眼看完。
    if let Ok(mut nvs) = EspNvs::new(nvs_part, NVS_NS, true) {
        let pin = ensure_ble_pin(&mut nvs);
        let lcd_pins = load_lcd_pins(&nvs);
        let _ = match ap_pass.as_deref() {
            // 加密熱點:網址那一行換成熱點密碼(連上後 captive portal 會自己跳設定頁)。
            Some(p) => show_provisioning_screen_secured(&lcd_pins, &ap_ssid, p, &pin),
            None => show_provisioning_screen(&lcd_pins, &ap_ssid, &ap_ip_str, &pin),
        };
    }

    // 熱點模式不能是死路。原本這裡是永遠不結束的迴圈 ——
    // 只要開機當下 WiFi 剛好不通(AP 還在重開、公司網路晚幾分鐘才好、
    // 停電後路由器比裝置慢開機),裝置就永久停在熱點模式,
    // 除非有人親自去拔插電源。對「掛在牆上沒人管」的裝置來說這是致命的。
    //
    // 改成:每 5 分鐘自動重開機一次,重開就會重新跑一輪完整的 WiFi 連線流程。
    // 用重開而不是就地重連,是因為 WiFi 驅動在 AP 模式與 STA 模式之間切換
    // 容易留下難以預期的狀態,重開最乾淨也最可靠。
    //
    // 已經有人連進設定門戶時不重開 —— 正在打字設定的人被重開會很惱火。
    const SOFTAP_RETRY_SECS: u64 = 300;
    let mut waited = 0u64;
    loop {
        std::thread::sleep(Duration::from_secs(1));
        feed_watchdog();
        waited += 1;
        if waited >= SOFTAP_RETRY_SECS {
            if portal_active_recently() {
                // 有人在設定,再等一輪,不要打斷他
                waited = SOFTAP_RETRY_SECS - 60;
                continue;
            }
            info!("熱點模式已 {} 秒,重開機再試一次 WiFi", waited);
            std::thread::sleep(Duration::from_secs(1));
            unsafe { esp_idf_svc::sys::esp_restart(); }
        }
    }
}

// ===== Main =====

// 讓韌體映像帶上有意義的 app descriptor(版本/專案名/編譯時間/IDF 版本)。
// 沒有這行的話描述元是空的:版本欄只有 "1"、專案名是 "libespidf",OTA 更新
// 拿不到可比對的版本號 —— 而 OTA 是本專案主打功能之一。
esp_idf_svc::sys::esp_app_desc!();

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    info!("=== {} ESP32-S3 AI Assistant v{} ===", BOT_NAME, FW_VERSION);
    info!("上次重啟原因: {}", reset_reason_str());

    // 7x24 無人值守底線:一開機就把主 task 註冊進 task WDT(90 秒,配 WDT_PANIC 自動重啟)。
    // 之前 feed_watchdog() 只 reset 沒 add → WDT 根本沒在監看主迴圈,真卡死=永久磚化;
    // 開機那串 "task_wdt: task not found" 洗版也是同一根因。
    let wdt_rc = unsafe { esp_idf_svc::sys::esp_task_wdt_add(std::ptr::null_mut()) };
    if wdt_rc == 0 {
        let cur = unsafe { esp_idf_svc::sys::xTaskGetCurrentTaskHandle() };
        MAIN_TASK_HANDLE.store(cur as *mut core::ffi::c_void, Ordering::Relaxed);
        info!("主 task 已註冊 task WDT (60s, 逾時自動重啟)");
    } else {
        warn!("esp_task_wdt_add 失敗: {}", wdt_rc);
    }

    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
    let nvs_part = EspDefaultNvsPartition::take()?;
    let nvs_part_ble = nvs_part.clone();
    let mut nvs = EspNvs::new(nvs_part.clone(), NVS_NS, true)?;
    seed_default_wifi(&mut nvs);

    let cfg_result = load_config(&mut nvs);
    let nvs_part_portal = nvs_part.clone();

    // WiFi - 多網路自動切換(最多 5 組)
    // 傳 None = 驅動用 WIFI_STORAGE_RAM，不再把 STA 設定寫進 NVS。
    // 我們自己的程式本來就把 SSID/密碼存在 NVS，驅動那份是多餘的；
    // 而 NVS 只有 24KB，裝了 24 個技能後幾乎滿了，驅動一寫就 ESP_ERR_NVS_NOT_ENOUGH_SPACE，
    // set_configuration 失敗 → 每個網路 20ms 就跳過 → 整台連不上網（2026-09-09 實機抓到）。
    let mut wifi = EspWifi::new(peripherals.modem, sysloop.clone(), None)?;
    let _ = nvs_part; // 仍給 BLE/門戶用的 clone 在上面已經取走

    // ── 提前啟動 BLE 配網 ──
    // BLE 必須在 WiFi 之前就緒，這樣「全新/無 WiFi」的裝置也能被手機 App 掃到並設定。
    // (修正先前架構: 舊版 BLE 只有 WiFi 連上後才啟動 → 沒 WiFi 的裝置永遠連不到。)
    // start_ble_provisioning 為非阻塞: 設定完即返回, 回呼跑在 NimBLE 自己的 task。
    // 確保配對 PIN 存在 (首次開機 → 產生隨機 6 位數，僅存 NVS、不寫死在韌體)
    let ble_pin_display = ensure_ble_pin(&mut nvs);
    info!("BLE: 裝置名 '{}' 配對 PIN: {}", ble_device_name(), ble_pin_display);

    BLE_NVS_PART.set(nvs_part_ble).ok();
    if !BLE_PROVISIONING_ENABLED {
        info!("BLE: 配網已停用(BLE_PROVISIONING_ENABLED=false),改用熱點設定頁或 USB 設定");
    } else if let Some(part) = BLE_NVS_PART.get() {
        BLE_INITIALIZED.store(true, Ordering::SeqCst);
        start_ble_provisioning(part.clone());
        info!("BLE: 配網已啟動，手機 App 現在即可連線設定");
    } else {
        warn!("BLE: NVS partition 不可用，BLE 停用");
    }

    // 使用者主動要求進設定模式(/setup 或控制台按鈕)。設定完好、WiFi 也連得上時,
    // 本來只能靠 /wifi clear 把設定砍掉才進得去 —— 代價是所有 WiFi 都要重打。
    // 這裡改成一次性旗標:開機讀到就先清掉再進配網模式,下次重開自動回正常運作。
    if nvs_get(&nvs, FORCE_SETUP_KEY).as_deref() == Some("1") {
        let _ = nvs.remove(FORCE_SETUP_KEY);
        warn!("使用者要求進入設定模式");
        run_softap_portal_mode(&mut wifi, nvs_part_portal, "使用者要求");
    }

    // 全新裝置(NVS 沒設定):零 USB 首次配網 —— 發 SoftAP 熱點 + captive portal,
    // 同時開背景序列配網執行緒(USB 設定工具依然可用)。存檔後裝置自動重啟。
    let mut cfg = match cfg_result {
        Ok(c) => { info!("Config loaded (WiFi: {})", c.wifi[0].0); c }
        Err(e) => {
            warn!("設定未完成 ({e}),進入 SoftAP 配網模式");
            let np = nvs_part_portal.clone();
            let _ = std::thread::Builder::new().name("serial_prov".into()).spawn(move || {
                if let Ok(mut nvs) = EspNvs::new(np, NVS_NS, true) {
                    if serial_provision(&mut nvs).is_ok() {
                        info!("序列配網完成,重啟...");
                        std::thread::sleep(Duration::from_millis(300));
                        unsafe { esp_idf_svc::sys::esp_restart(); }
                    }
                }
            });
            run_softap_portal_mode(&mut wifi, nvs_part_portal, "尚未設定");
        }
    };

    let wifi_nets: Vec<&(String, String)> = cfg.wifi.iter().filter(|(s, _)| !s.is_empty()).collect();
    const WIFI_MAX_ROUNDS: u32 = 3;
    let mut wifi_connected = false;
    let mut wifi_retry_round = 0u32;
    if wifi_nets.is_empty() {
        warn!("WiFi: 尚未設定任何網路 → 進入 BLE 配網模式");
    }
    'wifi_retry: while !wifi_nets.is_empty() && wifi_retry_round < WIFI_MAX_ROUNDS {
        if wifi_retry_round > 0 {
            let backoff = core::cmp::min(5 * wifi_retry_round, 30);
            warn!("WiFi: 第 {} 輪全部失敗，{}秒後重試...", wifi_retry_round, backoff);
            for _ in 0..backoff {
                std::thread::sleep(Duration::from_secs(1));
                feed_watchdog();
            }
        }
        wifi_retry_round += 1;
        for (idx, (ssid, pass)) in wifi_nets.iter().enumerate() {
            info!("WiFi[{}/{}] 第{}輪: 嘗試連接 {}...", idx + 1, wifi_nets.len(), wifi_retry_round, ssid);
            if wifi.is_started().unwrap_or(false) { let _ = wifi.stop(); }
            let ssid_hs = match ssid.as_str().try_into() {
                Ok(s) => s,
                Err(_) => { warn!("SSID {} 太長，跳過", ssid); continue; }
            };
            let pass_hs = match pass.as_str().try_into() {
                Ok(p) => p,
                Err(_) => { warn!("PASS 太長，跳過"); continue; }
            };
            if let Err(e) = wifi.set_configuration(&Configuration::Client(ClientConfiguration {
                ssid: ssid_hs,
                password: pass_hs,
                ..Default::default()
            })) { warn!("set_configuration 失敗 {}: {:?}", ssid, e); continue; }
            let _ = wifi.start();
            let _ = wifi.connect();
            for _ in 0..WIFI_CONNECT_TIMEOUT_SECS {
                std::thread::sleep(Duration::from_secs(1));
                feed_watchdog();
                if wifi.is_connected().unwrap_or(false) {
                    if let Ok(ip) = wifi.sta_netif().get_ip_info() {
                        if !ip.ip.is_unspecified() {
                            // 連到 IP 了,但要確認真的有網際網路(有些 WiFi 連得上卻沒網)
                            if internet_reachable() {
                                info!("WiFi 已連接且有網路！SSID={} IP={} (第{}輪)", ssid, ip.ip, wifi_retry_round);
                                remember_connection(&mut nvs, ssid, &ip.ip.to_string());
                                wifi_connected = true;
                                break 'wifi_retry;
                            } else {
                                warn!("WiFi {} 連上但無網際網路，改試下一個網路..", ssid);
                                let _ = wifi.disconnect();
                                break; // 跳出此網路等待迴圈,換下一個
                            }
                        }
                    }
                }
            }
            if !wifi.is_connected().unwrap_or(false) {
                warn!("WiFi {} 連線超時，改試下一個網路..", ssid);
            }
            let _ = wifi.disconnect();
        }
    }

    // WiFi 連不上 → 不卡死:發 SoftAP 熱點 + 設定門戶(BLE 也持續可用)。
    // 手機連熱點 → captive portal 自動跳出 http://10.1.1.1 設定頁 → 存檔重啟。
    if !wifi_connected {
        run_softap_portal_mode(&mut wifi, nvs_part_portal, "WiFi 連不上");
    }

    // 連上 WiFi:同一個設定門戶跑在區網 IP(換網路設定依然都在,畫面一樣)。
    let _portal_server = match start_config_portal(nvs_part_portal.clone()) {
        Ok(s) => {
            let ip = nvs_get(&nvs, "wifi_cur_ip").unwrap_or_default();
            info!("設定門戶: http://{}/ (區網)", ip);
            Some(s)
        }
        Err(e) => { warn!("設定門戶啟動失敗: {:?}", e); None }
    };

    // NTP time sync
    info!("Syncing NTP...");
    let _sntp = esp_idf_svc::sntp::EspSntp::new_default();
    if let Err(e) = &_sntp {
        warn!("SNTP init failed: {:?}, time may be inaccurate", e);
    }
    let mut ntp_wait = 0;
    loop {
        let now = now_epoch();
        if now > 1700000000 {
            info!("NTP synced! Epoch: {}", now);
            break;
        }
        ntp_wait += 1;
        // 對時不卡開機,但也不能太短。系統時間停在 1970 時,TLS 憑證驗證會判定
        // 「憑證尚未生效」而全部失敗 —— Telegram、Gemini 一個都連不上,
        // 但螢幕顯示已連線、IP 也正常,看起來就是「連上了卻不能用」。
        // 訪客網路第一次 NTP 往返超過 5 秒很常見,拉到 12 秒。
        // 真的沒同步也不卡開機:SNTP 背景會持續重試,連線看門狗會兜底。
        if ntp_wait > 12 {
            warn!("NTP sync timeout, continuing without sync");
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
    }

    let now = now_epoch();
    let (h, m) = epoch_to_hhmm(now);
    info!("Local time: {:02}:{:02} (UTC+8)", h, m);

    let mut state = AppState::new(now);

    // Load persistent memories from NVS
    load_memories_from_nvs(&nvs, &mut state);
    info!("Loaded {} memories from NVS", state.memories.len());

    // Load skills from NVS
    state.skills = load_skills_from_nvs(&nvs);
    info!("Loaded {} skills from NVS", state.skills.len());

    if let Some(saved_model) = nvs_get(&nvs, "model_pref") {
        if let Some(valid_model) = normalize_model_selection(&saved_model) {
            state.current_model = valid_model.to_string();
        }
    }
    if let Some(saved_voice_mode) = nvs_get(&nvs, "voice_mode") {
        if let Some(valid_voice_mode) = normalize_voice_mode(&saved_voice_mode) {
            state.voice_mode = valid_voice_mode.to_string();
        }
    }
    if let Some(saved_wake_phrase) = nvs_get(&nvs, "wake_phrase") {
        state.wake_phrase = saved_wake_phrase;
    }
    if let Some(saved_wake_enabled) = nvs_get_bool(&nvs, "wake_enabled") {
        state.wake_enabled = saved_wake_enabled;
    }
    info!("Runtime prefs: model={}, voice_mode={}", state.current_model, state.voice_mode);

    let deleted = deleted_builtins(&nvs);
    if ensure_builtin_skills(&mut state.skills, &deleted) {
        save_skills_to_nvs(&mut nvs, &state.skills);
        info!("Ensured {} builtin skills", state.skills.len());
    }

    // Mark OTA partition as valid (rollback protection)
    unsafe {
        let ota_err = esp_idf_svc::sys::esp_ota_mark_app_valid_cancel_rollback();
        if ota_err == 0 { info!("OTA: app marked valid"); }
    }

    // Load reminders and auto tasks from NVS (persisted across reboots)
    load_reminders_from_nvs(&nvs, &mut state, now);
    // 使用者指定的 temperature（"auto" 或數字）。auto 就維持 0 交給系統決定。
    if let Some(v) = nvs_get(&nvs, AI_TEMP_KEY) {
        if let Ok(f) = v.trim().parse::<f32>() {
            if (0.0..=1.0).contains(&f) {
                AI_TEMP_FIXED_X100.store((f * 100.0) as u32, Ordering::Relaxed);
            }
        }
    }
    // 相機擺放方向要在第一次拍照前就載入，否則開機自檢那張會是舊方向
    CAMERA_FLIP_MODE.store(
        nvs_get(&nvs, CAMERA_FLIP_KEY).and_then(|v| v.parse().ok()).unwrap_or(0),
        Ordering::Relaxed,
    );
    // 解析度必須在相機 init 之前載入 —— frame_size 是 init 時決定的,
    // 之後再改要重新 init 才會生效。
    CAMERA_RES_MODE.store(
        nvs_get(&nvs, CAMERA_RES_KEY).and_then(|v| v.parse().ok()).unwrap_or(4),
        Ordering::Relaxed,
    );
    load_auto_tasks_from_nvs(&nvs, &mut state, now);

    // Default daily reminders ??only add if none were loaded from NVS
    if state.reminders.is_empty() {
        state.add_reminder(ReminderType::Daily, 0, 8, 0,
            "早安！記得查看今天的天氣狀況，也可以輸入 /briefing 取得每日報告！", now);
        state.add_reminder(ReminderType::Daily, 0, 12, 0,
            "午安！記得吃飯、喝水休息一下，我也可以幫你處理各種事情", now);
        state.add_reminder(ReminderType::Daily, 0, 18, 30,
            "晚安！今天辛苦了，可以幫你整理一下今天的紀錄，繼續加油！", now);
        save_reminders_to_nvs(&mut nvs, &state);
        info!("Default reminders set (08:00, 12:00, 18:30) and saved to NVS");
    } else {
        info!("Restored {} reminders from NVS, {} auto tasks", state.reminders.len(), state.auto_tasks.len());
    }

    let lcd_pins = load_lcd_pins(&nvs);
    info!("LCD config: {}", lcd_pins.status_text().replace('\n', " | "));
    let _ = show_boot_screen(&lcd_pins);
    let audio_pins = load_audio_pins(&nvs);
    info!("Audio config: {}", audio_pins.status_text().replace('\n', " | "));
    info!("Boot audio tone test skipped; use /audio tone for manual verification");
    match board_hw::capture_mic_snapshot(&audio_pins, 800) {
        Ok(snapshot) => info!(
            "Boot mic snapshot OK: rms={} peak={} samples={}",
            snapshot.rms, snapshot.peak, snapshot.samples
        ),
        Err(err) => warn!("Boot mic snapshot failed: {}", err),
    }
    #[cfg(esp_idf_comp_espressif__esp32_camera_enabled)]
    {
        let camera_pins = load_camera_pins(&nvs);
        match capture_camera_jpeg(&camera_pins) {
            Ok(jpeg) => info!("Boot camera test OK: {} KB", jpeg.len() / 1024),
            Err(err) => warn!("Boot camera test failed: {}", err),
        }
    }

    // BLE 配網已於開機早期啟動 (WiFi 迴圈之前)，此處不再重複初始化。

    // BOOT button (GPIO0) ??Push-to-Talk
    std::thread::Builder::new()
        .name("btn".into())
        .stack_size(4096)
        .spawn(|| {
            unsafe {
                esp_idf_sys::gpio_reset_pin(0);
                esp_idf_sys::gpio_set_direction(0, esp_idf_sys::gpio_mode_t_GPIO_MODE_INPUT);
                esp_idf_sys::gpio_set_pull_mode(0, esp_idf_sys::gpio_pull_mode_t_GPIO_PULLUP_ONLY);
            }
            let mut prev = 1;
            loop {
                let level = unsafe { esp_idf_sys::gpio_get_level(0) };
                if prev == 1 && level == 0 {
                    BUTTON_PRESSED.store(true, Ordering::SeqCst);
                }
                prev = level;
                std::thread::sleep(Duration::from_millis(50));
            }
        })?;
    info!("BOOT button (GPIO0) Push-to-Talk ready");

    let mut last_id: i64 = match prepare_telegram_polling(&cfg.tg_token, &nvs) {
        Ok(id) => {
            info!("Telegram polling ready, last persisted update id={}", id);
            id
        }
        Err(err) => {
            warn!("Telegram polling prepare failed: {}", err);
            load_last_telegram_update_id(&nvs)
        }
    };

    // Boot notification
    let mut chat_id_num: i64 = cfg.chat_id.parse().unwrap_or(0);
    let heap_free = unsafe { esp_idf_svc::sys::esp_get_free_heap_size() };
    let bot_user = fetch_bot_username(&cfg.tg_token).unwrap_or_else(|_| "unknown".to_string());
    // 快取 bot username 給控制台的「一鍵綁定」按鈕用(避免每次開分頁都呼叫 Telegram API)
    if bot_user != "unknown" {
        let _ = nvs.set_str("tg_bot_username", &bot_user);
    }

    // ESP-SR WakeNet init (local wake word detection)
    let sr_status = match init_wakenet() {
        Ok(word) => format!("ESP-SR: \u{2705}{}", word),
        Err(e) => {
            warn!("ESP-SR init failed: {}", e);
            format!("ESP-SR: \u{274c}{}", e)
        }
    };

    let boot_msg = format!(
        "\u{1f916} NANA v{} online!\n\
         Bot: @{}\n\
         Model: {}\n\
         Voice: {}\n\
         Wake: {} ({})\n\
         {}\n\
         Time: {:02}:{:02}\n\
         Free RAM: {}KB\n\
         Reset: {}\n\
         Memories: {}\n\
         Skills: {}\n\
         PC Driver: USB Ready\n\
         OTA: Ready\n\
         TTS Cache: {}\n\
         /help for commands",
        FW_VERSION,
        bot_user,
        state.current_model,
        state.voice_mode,
        if state.wake_enabled { "on" } else { "off" },
        state.wake_phrase,
        sr_status,
        h, m,
        heap_free / 1024,
        reset_reason_str(),
        state.memories.len(), state.skills.len(),
        TTS_CACHE_MAX,
    );
    if chat_id_num == 0 {
        info!("尚未綁定 Telegram(chat_id 空白) — 略過開機通知,等使用者在控制台按「加好友並綁定」");
    } else {
        match send_telegram(&cfg.tg_token, chat_id_num, &boot_msg) {
            Ok(()) => info!("Boot notification sent!"),
            Err(e) => error!("Boot notify failed: {:?}", e),
        }
    }
    // 開機完成後顯示「怎麼進控制台」而不是一句 ready ——
    // 使用者這時最需要知道的是 IP 和 PIN,而在還沒綁 Telegram 之前
    // 螢幕是他唯一查得到的地方。
    {
        let ip = nvs_get(&nvs, "wifi_cur_ip").unwrap_or_default();
        let pin = nvs_get(&nvs, "ble_pin").unwrap_or_default();
        // 要顯示「實際連上的」網路。原本取設定裡第一個非空的 SSID,
        // 一旦是靠備援網路連上的,螢幕就會寫著另一個網路的名字 ——
        // 現場看到不符會直接懷疑整台裝置。連線時存的那個才是真的。
        let cur = nvs_get(&nvs, "wifi_cur_ssid").unwrap_or_default();
        let ssid = if !cur.trim().is_empty() {
            cur.clone()
        } else {
            cfg.wifi.iter().map(|(s, _)| s.as_str())
                .find(|s| !s.is_empty()).unwrap_or("WiFi").to_string()
        };
        let ssid = ssid.as_str();
        if ip.is_empty() || pin.is_empty() {
            let _ = show_ready_screen(&lcd_pins);
        } else {
            let _ = show_connected_screen(&lcd_pins, ssid, &ip, &pin);
        }
    }

    // BLE 配網已於開機早期啟動 (WiFi 之前)，此處不再重複初始化。

    // Main loop
    info!("=== {} Telegram Bot ready ===", BOT_NAME);
    let mut consecutive_errors: u32 = 0;
    let mut last_reminder_check: u64 = now;
    let mut last_wifi_health_check: u64 = now;
    // 最後一次「確實連得出去」是開機後第幾秒。用開機時間而非系統時間,
    // 因為 NTP 一對時系統時間會從 1970 跳到現在,差值會瞬間爆掉。
    let mut last_online: u32 = uptime_secs();
    let mut last_reassoc_attempt: u32 = 0;
    let serial_cmd_rx = spawn_serial_command_reader();

    // Send handshake to PC driver so it knows we're alive (token no longer shared for security)
    usb_send_pc_cmd("handshake", &serde_json::json!({"fw": FW_VERSION}), chat_id_num);

    // heap 趨勢監控 + 凌晨 4 點保底重啟(壓住記憶體碎片這種跑數週才發作的問題)
    let boot_instant = std::time::Instant::now();
    // 上報到使用者 API 時的裝置識別。跟熱點/BLE 同名,每台唯一,
    // 使用者那邊多台裝置寫進同一個資料庫時靠這個分辨。
    let device_name = ble_device_name();
    let mut last_heap_report = std::time::Instant::now();
    // 待機換臉:沒人講話時每 5 分鐘重畫一次,她才不會是一張定格照片
    let mut last_idle_face = std::time::Instant::now();
    let mut idle_face_tick: u32 = 0;

    loop {
        // 每 10 分鐘記錄一次 heap;內部 RAM 低於 20KB 視為瀕臨耗盡 → 主動重啟自救
        if last_heap_report.elapsed() >= Duration::from_secs(600) {
            last_heap_report = std::time::Instant::now();
            let free_total = unsafe { esp_idf_svc::sys::esp_get_free_heap_size() };
            let min_total = unsafe { esp_idf_svc::sys::esp_get_minimum_free_heap_size() };
            let free_internal = unsafe {
                esp_idf_svc::sys::heap_caps_get_free_size(esp_idf_svc::sys::MALLOC_CAP_INTERNAL)
            };
            info!("Heap: free={}KB min_ever={}KB internal={}KB",
                free_total / 1024, min_total / 1024, free_internal / 1024);
            if free_internal < 20 * 1024 {
                warn!("內部 RAM 僅剩 {}B,主動重啟自救", free_internal);
                let _ = send_telegram(&cfg.tg_token, chat_id_num, "⚠️ 記憶體不足，自動重啟一次維持穩定");
                std::thread::sleep(Duration::from_secs(2));
                unsafe { esp_idf_svc::sys::esp_restart(); }
            }
        }
        // 待機換臉:每 5 分鐘挑一張新表情重畫。暫停中或串流中不做 ——
        // 串流時 SPI 排線正忙,插一次重畫會拖慢畫面。
        if last_idle_face.elapsed() >= Duration::from_secs(300)
            && !PAUSED.load(Ordering::Relaxed)
            && !STREAM_ACTIVE.load(Ordering::Relaxed)
        {
            last_idle_face = std::time::Instant::now();
            idle_face_tick = idle_face_tick.wrapping_add(1);
            if let Err(e) = show_idle_face(&lcd_pins, idle_face_tick) {
                warn!("待機換臉失敗: {:?}", e);
            }
        }
        // 凌晨 4 點保底重啟(長輩熟睡時段;uptime>1h 防止重啟後立刻再觸發)
        {
            let (h, m) = epoch_to_hhmm(now_epoch());
            if h == 4 && m == 0 && boot_instant.elapsed() >= Duration::from_secs(3600) {
                info!("凌晨 4 點例行重啟(記憶體保養)");
                std::thread::sleep(Duration::from_secs(1));
                unsafe { esp_idf_svc::sys::esp_restart(); }
            }
        }
        while let Ok(serial_cmd) = serial_cmd_rx.try_recv() {
            // Filter PC driver JSON responses (start with '{' and contain "id")
            if serial_cmd.starts_with('{') {
                if let Ok(obj) = serde_json::from_str::<serde_json::Value>(&serial_cmd) {
                    if obj.get("id").is_some() || obj.get("ok").is_some() {
                        DRIVER_CONNECTED.store(true, Ordering::Relaxed);
                        info!("PC driver response: {}", serial_cmd);
                        continue;
                    }
                }
            }
            // Driver requests re-handshake after connecting
            if serial_cmd.trim() == "/driver_ping" {
                info!("Driver ping received, re-sending handshake");
                usb_send_pc_cmd("handshake", &serde_json::json!({"fw": FW_VERSION}), chat_id_num);
                continue;
            }
            info!("Serial input ({} chars)", serial_cmd.len());
            handle_text(&cfg, &mut nvs, &mut state, chat_id_num, &serial_cmd);
        }

        // 門戶(網頁)送來的指令:與序列/Telegram 走同一套 handle_text
        let portal_cmds: Vec<String> = {
            let mut q = PORTAL_CMDS.lock().unwrap_or_else(|e| e.into_inner());
            q.drain(..).collect()
        };
        for pc in portal_cmds {
            info!("Portal cmd ({} chars)", pc.len());
            handle_text(&cfg, &mut nvs, &mut state, chat_id_num, &pc);
        }
        // 刷新門戶 /api/dump 的狀態快照(每輪 ~8 秒一次,成本低)
        refresh_portal_state_json(&state, &nvs);

        // Push-to-Talk: BOOT button (GPIO0)
        if BUTTON_PRESSED.swap(false, Ordering::SeqCst) {
            info!("PTT button pressed! Recording 5 seconds...");
            let _ = send_telegram(&cfg.tg_token, chat_id_num, "\u{1f3a4} PTT: \u{6b63}\u{5728}\u{807d}...");
            let _ = show_reply_screen(&lcd_pins, "\u{1f3a4} Listening...");
            std::thread::sleep(Duration::from_millis(300));
            match capture_and_transcribe_mic(&cfg, &nvs, &mut state, 5000) {
                Ok(transcript) if !transcript.trim().is_empty() => {
                    info!("PTT transcribed ({} chars)", transcript.len());
                    let _ = send_telegram(
                        &cfg.tg_token, chat_id_num,
                        &format!("\u{1f3a4} PTT: {}", transcript),
                    );
                    handle_text(&cfg, &mut nvs, &mut state, chat_id_num, &voice_as_chat(&transcript));
                }
                Ok(_) => {
                    info!("PTT: silence");
                    let _ = show_ready_screen(&lcd_pins);
                }
                Err(err) => {
                    warn!("PTT transcribe failed: {}", err);
                    let _ = show_reply_screen(&lcd_pins, "PTT error");
                }
            }
        }

        // Check reminders every 30s
        let now = now_epoch();
        let (hour, minute) = epoch_to_hhmm(now);
        let today = local_day_number(now);
        if hour == 8 && minute < 5 && state.last_briefing_day != today {
            state.last_briefing_day = today;
            let briefing = build_daily_briefing(&state, now);
            if let Err(e) = send_telegram(&cfg.tg_token, chat_id_num, &briefing) {
                error!("Morning briefing send failed: {:?}", e);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        if now - last_reminder_check >= 30 {
            last_reminder_check = now;
            let fired = state.check_reminders(now);
            if !fired.is_empty() {
                save_reminders_to_nvs(&mut nvs, &state); // persist Once reminders becoming inactive
            }
            for msg in fired {
                if let Err(e) = send_telegram(&cfg.tg_token, chat_id_num, &msg) {
                    error!("Reminder send failed: {:?}", e);
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }

        // Execute auto tasks (skip when paused)
        if !PAUSED.load(Ordering::Relaxed) {
            let task_ids: Vec<u32> = state.auto_tasks.iter()
                .filter(|t| t.active && now >= t.next_trigger)
                .map(|t| t.id)
                .collect();
            for tid in task_ids {
                if PAUSED.load(Ordering::Relaxed) { break; }
                if let Some(idx) = state.auto_tasks.iter().position(|t| t.id == tid) {
                    state.auto_tasks[idx].next_trigger = now + state.auto_tasks[idx].interval_secs;
                    let task = state.auto_tasks[idx].clone();
                    info!("AutoTask #{} firing: action={} prompt_len={}", task.id, task.action, task.prompt.len());
                    // 這一輪成功了沒。任何需要網路/AI 的動作都可能失敗(最常見是撞到
                    // 每 60 秒 5 次的 AI 速率上限 —— 使用者剛好在拍照時間點附近講了
                    // 幾句話就會發生),失敗就排一次短間隔重試,不要整輪丟掉。
                    let mut succeeded = true;
                    match task.action.as_str() {
                        "camera" => {
                            #[cfg(esp_idf_comp_espressif__esp32_camera_enabled)]
                            {
                                let camera_pins = load_camera_pins(&nvs);
                                match capture_camera_jpeg(&camera_pins) {
                                    Ok(jpeg) => {
                                        match describe_image_with_gemini(&cfg.gemini_key, &mut state, &jpeg, &task.prompt) {
                                            Ok(result) => {
                                                let msg = format!("\u{1f916} AutoTask#{}: {}", task.id, result);
                                                let _ = send_telegram(&cfg.tg_token, chat_id_num, &msg);
                                                // 上報到使用者自己的 API(沒設定就跳過)。
                                                // 失敗要計數並在連續失敗時通知 —— 否則資料默默掉了,
                                                // 使用者以為都有進資料庫。
                                                report_and_track(
                                                    &nvs, &cfg.tg_token, chat_id_num,
                                                    &device_name, task.id, "camera",
                                                    &task.prompt, &extract_value(&result),
                                                    &result, true, true);
                                                // 使用者開了「附照片」才送。判讀當下的原始畫面
                                                // 是唯一能事後驗證「它到底讀對沒」的東西。
                                                if report_photo_enabled(&nvs) {
                                                    let ts = now_epoch();
                                                    if let Err(e) = report_photo(
                                                        &nvs, &device_name, task.id, ts, &jpeg)
                                                    {
                                                        warn!("AutoTask#{} 照片上報失敗: {:?}", task.id, e);
                                                    }
                                                    let cap = format!("AutoTask#{} 判讀當下畫面", task.id);
                                                    let _ = send_telegram_photo_jpeg(
                                                        &cfg.tg_token, chat_id_num, &jpeg, &cap);
                                                }
                                                if task.speak && should_speak_onboard(&state.voice_mode) {
                                                    let speech = speech_text_for_mode(&state.voice_mode, &result);
                                                    if let Err(e) = speak_onboard_text(&nvs, &speech) {
                                                        warn!("AutoTask#{} TTS: {:?}", task.id, e);
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                warn!("AutoTask#{} vision failed: {:?}", task.id, e);
                                                succeeded = false;
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        warn!("AutoTask#{} camera failed: {:?}", task.id, e);
                                        succeeded = false;
                                    }
                                }
                            }
                            #[cfg(not(esp_idf_comp_espressif__esp32_camera_enabled))]
                            {
                                warn!("AutoTask#{}: camera not enabled in firmware", task.id);
                            }
                        }
                        "watch" => {
                            // 條件式監看:拍照 → 叫 Gemini 用 JSON 回答「符合了沒」→
                            // 只有符合才通知。不符合時「完全不出聲」是這個 action 的重點,
                            // 不然每 5 分鐘收一則「目前正常」等於沒人會看。
                            #[cfg(esp_idf_comp_espressif__esp32_camera_enabled)]
                            {
                                let camera_pins = load_camera_pins(&nvs);
                                match capture_camera_jpeg(&camera_pins) {
                                    Ok(jpeg) => {
                                        let vision_prompt = build_watch_prompt(&task.prompt);
                                        match describe_image_with_gemini(&cfg.gemini_key, &mut state, &jpeg, &vision_prompt) {
                                            Ok(raw) => match parse_watch_verdict(&raw) {
                                                Some(verdict) if verdict.hit => {
                                                    let (_, cond) = strip_roi_prefix(&task.prompt);
                                                    let msg = format!(
                                                        "\u{1f6a8} 監看#{} 觸發：{}\n{}",
                                                        task.id, cond, verdict.detail);
                                                    let _ = send_telegram(&cfg.tg_token, chat_id_num, &msg);
                                                    report_and_track(
                                                        &nvs, &cfg.tg_token, chat_id_num,
                                                        &device_name, task.id, "watch",
                                                        &cond, &extract_value(&verdict.detail),
                                                        &verdict.detail, true, true);
                                                    // 監看觸發是最需要證據的時刻:使用者收到「有人沒戴安全帽」
                                                    // 的通知,第一個反應一定是「照片呢」。
                                                    if report_photo_enabled(&nvs) {
                                                        let ts = now_epoch();
                                                        if let Err(e) = report_photo(
                                                            &nvs, &device_name, task.id, ts, &jpeg)
                                                        {
                                                            warn!("Watch#{} 照片上報失敗: {:?}", task.id, e);
                                                        }
                                                        let cap = format!("監看#{} 觸發當下畫面", task.id);
                                                        let _ = send_telegram_photo_jpeg(
                                                            &cfg.tg_token, chat_id_num, &jpeg, &cap);
                                                    }
                                                    // 提示音也要看語音模式。使用者設 /voice off 的意思是
                                                    // 「不要出聲」,不是「不要唸字但可以嗶」—— 半夜監看觸發
                                                    // 時嗶一聲跟唸出來一樣吵。
                                                    if state.voice_mode != "off" {
                                                        let _ = play_test_tone(&load_audio_pins(&nvs));
                                                    }
                                                    if task.speak && should_speak_onboard(&state.voice_mode) {
                                                        if let Err(e) = speak_onboard_text(&nvs, &verdict.detail) {
                                                            warn!("Watch#{} TTS: {:?}", task.id, e);
                                                        }
                                                    }
                                                }
                                                // 沒觸發 → 安靜(這是設計,不是漏做)
                                                // 沒觸發也要上報 hit=false —— 對監控系統來說「這一輪檢查過了、
                                                // 沒事」本身就是一筆有價值的紀錄,否則資料庫裡只看得到異常,
                                                // 分不出「正常」與「根本沒在跑」。
                                                Some(v) => {
                                                    info!("Watch#{}: no hit", task.id);
                                                    let (_, cond) = strip_roi_prefix(&task.prompt);
                                                    report_and_track(
                                                        &nvs, &cfg.tg_token, chat_id_num,
                                                        &device_name, task.id, "watch",
                                                        &cond, &extract_value(&v.detail),
                                                        &v.detail, false, true);
                                                }
                                                None => {
                                                    warn!("Watch#{} unparseable verdict: {}", task.id,
                                                        raw.chars().take(120).collect::<String>());
                                                    succeeded = false;
                                                }
                                            },
                                            Err(e) => {
                                                warn!("Watch#{} vision failed: {:?}", task.id, e);
                                                succeeded = false;
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        warn!("Watch#{} camera failed: {:?}", task.id, e);
                                        succeeded = false;
                                    }
                                }
                            }
                            #[cfg(not(esp_idf_comp_espressif__esp32_camera_enabled))]
                            {
                                warn!("Watch#{}: camera not enabled in firmware", task.id);
                            }
                        }
                        "gpio" => {
                            // prompt = "PIN:STATE" 例如 "13:1" → 把該腳位設為高/低(供電/斷電)
                            let p: Vec<&str> = task.prompt.split(':').collect();
                            if p.len() == 2 {
                                if let (Ok(pin), Ok(st)) = (p[0].parse::<i32>(), p[1].parse::<u32>()) {
                                    unsafe {
                                        esp_idf_sys::gpio_set_direction(pin, esp_idf_sys::gpio_mode_t_GPIO_MODE_OUTPUT);
                                        esp_idf_sys::gpio_set_level(pin, if st != 0 { 1 } else { 0 });
                                    }
                                    let _ = send_telegram(&cfg.tg_token, chat_id_num,
                                        &format!("\u{26a1} AutoTask#{}: GPIO{} = {}", task.id, pin, if st != 0 { "供電 ON" } else { "斷電 OFF" }));
                                }
                            }
                        }
                        "alert" => {
                            // prompt = 訊息 → 發 Telegram + 提示音 + 大聲 TTS 唸出
                            let _ = send_telegram(&cfg.tg_token, chat_id_num,
                                &format!("\u{1f514} AutoTask#{}: {}", task.id, task.prompt));
                            // 同上:/voice off 是「不要出聲」,提示音也算出聲
                            if state.voice_mode != "off" {
                                let _ = play_test_tone(&load_audio_pins(&nvs));
                            }
                            if let Err(e) = speak_onboard_text(&nvs, &task.prompt) {
                                warn!("AutoTask#{} alert TTS: {:?}", task.id, e);
                            }
                        }
                        _ => {}
                    }

                    // 有界重試迴圈:失敗就排一次短間隔重試(最多 AUTOTASK_MAX_RETRIES 次),
                    // 額度用完就放棄這一輪、等下一個正常間隔,不會無限重試把額度燒光。
                    // 成功則把次數補回滿,下一輪重新算。
                    if let Some(i) = state.auto_tasks.iter().position(|t| t.id == tid) {
                        if succeeded {
                            state.auto_tasks[i].retries_left = AUTOTASK_MAX_RETRIES;
                            reset_vision_temp();
                            // 恢復正常就把連續失敗歸零;若先前已告警過,補一則恢復通知,
                            // 否則使用者只知道壞了、不知道什麼時候好的。
                            if state.auto_tasks[i].consecutive_failures >= AUTOTASK_ALERT_AFTER_ROUNDS {
                                let _ = send_telegram(&cfg.tg_token, chat_id_num,
                                    &format!("\u{2705} 任務#{} 已恢復正常運作", tid));
                            }
                            state.auto_tasks[i].consecutive_failures = 0;
                        } else if state.auto_tasks[i].retries_left > 0 {
                            state.auto_tasks[i].retries_left -= 1;
                            // 換個參數再試,不要用一模一樣的做法重跑 ——
                            // 同樣的圖 + 同樣的提示詞 + 同樣的 temperature,
                            // 重跑十次通常還是同一個結果,只是白花額度。
                            escalate_vision_temp();
                            // 這裡必須重新取時間。`now` 是主迴圈開頭取的,但任務本身
                            // (拍照 + 上傳 + AI 判讀)可能跑 30~60 秒才失敗 —— 用舊的
                            // now 算出來的 +70 秒,實際只剩幾秒就到期,「等過完整速率
                            // 視窗再試」根本不成立,反而更早撞上下一次額度不足。
                            let now_after = now_epoch();
                            state.auto_tasks[i].next_trigger = now_after + AUTOTASK_RETRY_DELAY_SECS;
                            warn!("AutoTask#{} 這輪失敗,{}秒後重試(剩 {} 次)",
                                  tid, AUTOTASK_RETRY_DELAY_SECS, state.auto_tasks[i].retries_left);
                        } else {
                            state.auto_tasks[i].retries_left = AUTOTASK_MAX_RETRIES;
                            reset_vision_temp(); // 這一輪放棄了,下一輪從基準重新開始
                            state.auto_tasks[i].consecutive_failures =
                                state.auto_tasks[i].consecutive_failures.saturating_add(1);
                            let fails = state.auto_tasks[i].consecutive_failures;
                            warn!("AutoTask#{} 重試用盡,放棄這一輪(連續第 {} 輪失敗)", tid, fails);
                            // 監控類任務靜默失效是最糟的情況:相機被遮住、鏡頭髒了、
                            // 網路斷了 —— 每輪都失敗但使用者什麼都收不到,還以為一切正常。
                            // 到門檻告警一次(用 == 不用 >=,避免之後每輪洗版)。
                            if fails == AUTOTASK_ALERT_AFTER_ROUNDS {
                                let _ = send_telegram(&cfg.tg_token, chat_id_num, &format!(
                                    "\u{26a0}\u{fe0f} 任務#{} 連續 {} 輪失敗,目前沒有在正常監控。\n\
                                     常見原因:相機被遮住或鏡頭髒了、網路不穩、AI 額度用盡。\n\
                                     排除後會自動恢復,恢復時我會再通知你。",
                                    tid, fails));
                            }
                        }
                    }
                }
            }
        }

        if state.wake_enabled && !PAUSED.load(Ordering::Relaxed) && now.saturating_sub(state.last_wake_check) >= WAKE_CHECK_INTERVAL_SECS {
            state.last_wake_check = now;
            match detect_wake_phrase(&cfg, &nvs, &mut state, 2200) {
                Ok(Some(transcript)) => {
                    if now.saturating_sub(state.last_wake_trigger) >= WAKE_TRIGGER_COOLDOWN_SECS {
                        state.last_wake_trigger = now;
                        info!("Wake phrase detected ({} chars)", transcript.len());
                        let _ = send_telegram(
                            &cfg.tg_token,
                            chat_id_num,
                            &format!("\u{1f3a4} Wake: {}", transcript),
                        );
                        let _ = show_reply_screen(&lcd_pins, "Wake detected");
                        if should_speak_onboard(&state.voice_mode) {
                            if let Err(err) = speak_onboard_text(&nvs, "\u{6211}\u{5728}\u{ff0c}\u{8acb}\u{8aaa}\u{3002}") {
                                warn!("Wake TTS failed: {}", err);
                            }
                        }
                        // Record follow-up command after wake
                        std::thread::sleep(Duration::from_millis(500));
                        let _ = show_reply_screen(&lcd_pins, "\u{1f3a4} Listening...");
                        match capture_and_transcribe_mic(&cfg, &nvs, &mut state, 4000) {
                            Ok(cmd) if !cmd.trim().is_empty() => {
                                info!("Wake cmd ({} chars)", cmd.len());
                                let _ = send_telegram(
                                    &cfg.tg_token, chat_id_num,
                                    &format!("\u{1f3a4} {}", cmd),
                                );
                                handle_text(&cfg, &mut nvs, &mut state, chat_id_num, &voice_as_chat(&cmd));
                            }
                            Ok(_) => info!("Wake follow-up: silence"),
                            Err(err) => warn!("Wake follow-up failed: {}", err),
                        }
                    }
                }
                Ok(None) => {}
                Err(err) => warn!("Wake listener failed: {}", err),
            }
        }

        // ── 連線看門狗 ──────────────────────────────────────────────
        // 針對「關聯著 AP 但實際不通」這個狀態。驅動層的 is_connected() 只代表
        // 跟 AP 完成關聯,不代表拿得到 IP、更不代表出得去。實務上很常見:
        // AP 正常但對外斷了、DHCP 租約過期沒續到、訪客網路把裝置踢到認證前的狀態。
        //
        // 這時候底下每一條重連判斷都以 is_connected()==false 為前提,全部不會觸發,
        // 裝置就永遠卡著 —— 螢幕顯示正常、有 IP、但收不到也送不出任何訊息,
        // 而且不會自己好,沒有上限、沒有通知、也不會重開。使用者看到的就是
        // 「有時候連不上,而且要拔電才會好」。
        //
        // 所以改用「多久沒有真的連出去過」當判準,不問驅動層連著沒有。
        // Telegram 409(兩台裝置共用同一個 bot token)不是網路問題。網路其實是通的,
        // 重開機解決不了,只會變成每 15 分鐘重開一次的迴圈 —— 而使用者手上就有兩塊板。
        // 所以把它當「線上」,問題交給 /status 與診斷紀錄呈現。
        if TG_CONFLICT.load(Ordering::Relaxed) {
            last_online = uptime_secs();
        }
        // 心跳取「getUpdates 成功」與「任何 HTTP 有回應」兩者較新的。只看前者的話，
        // 一輪任務跑超過 180 秒就會被強制斷線再誤發「WiFi 已恢復」——這是加看門狗時留下的洞。
        let last_seen = last_online.max(LAST_ONLINE.load(Ordering::Relaxed));
        let offline_for = uptime_secs().saturating_sub(last_seen);
        if offline_for >= OFFLINE_REBOOT_SECS {
            warn!("連線看門狗:已 {} 秒沒有任何一次成功對外連線 → 重開機", offline_for);
            state.diag.log("ERR", &format!("offline {}s -> reboot", offline_for));
            std::thread::sleep(Duration::from_millis(300));
            unsafe { esp_idf_svc::sys::esp_restart(); }
        }
        if offline_for >= OFFLINE_REASSOC_SECS
            && uptime_secs().saturating_sub(last_reassoc_attempt) >= OFFLINE_REASSOC_SECS
        {
            last_reassoc_attempt = uptime_secs();
            warn!("連線看門狗:已 {} 秒不通,強制重跑連線流程", offline_for);
            state.diag.log("WARN", &format!("offline {}s -> forced reassoc", offline_for));
            // 主動斷開。底下的重連邏輯都要 is_connected()==false 才會動,
            // 不先斷開的話它們會繼續認為一切正常。
            let _ = wifi.disconnect();
            let _ = wifi.stop();
            last_wifi_health_check = 0; // 讓健康檢查這一輪立刻觸發
        }

        // Proactive WiFi health check (every WIFI_HEALTH_CHECK_SECS)
        // Catches silent drops before Telegram polling fails
        let now = now_epoch();
        if now.saturating_sub(last_wifi_health_check) >= WIFI_HEALTH_CHECK_SECS {
            last_wifi_health_check = now;
            if !wifi.is_connected().unwrap_or(false) {
                warn!("WiFi health check: 連線已斷，開始輪播重連..");
                // 現讀 NVS,不用開機快照 —— 現場剛用 /wifi set 加的網路才試得到。
                let recon_nets = wifi_nets_from_nvs(&nvs);
                let mut recon_round = 0u32;
                // 上限 10 輪(~5 分鐘):路由器沒回來就先放手讓主迴圈降級續跑
                // (門戶指令/提醒/喇叭都還能動),下個健康檢查週期再試。
                // 沒有上限的話這裡會永久凍結整台機器(還一直餵狗,WDT 也救不了)。
                'health_recon: while recon_round < 10 {
                    if recon_round > 0 {
                        let backoff = core::cmp::min(5 * recon_round, 30);
                        warn!("Health reconnect: 第{}輪全部失敗，{}秒後重試...", recon_round, backoff);
                        for _ in 0..backoff {
                            std::thread::sleep(Duration::from_secs(1));
                            feed_watchdog();
                        }
                    }
                    recon_round += 1;
                    for (ssid, pass) in recon_nets.iter() {
                        info!("Health reconnect[第{}輪]: 嘗試 {}...", recon_round, ssid);
                        if wifi.is_started().unwrap_or(false) { let _ = wifi.stop(); }
                        // unwrap_or_default() 會把過長的 SSID 悄悄變成空字串,
                        // 然後拿空 SSID 去連 —— 失敗訊息完全看不出真正原因。跳過才對。
                        let ssid_hs = match ssid.as_str().try_into() {
                            Ok(v) => v,
                            Err(_) => { warn!("SSID {} 太長,跳過", ssid); continue; }
                        };
                        let pass_hs = match pass.as_str().try_into() {
                            Ok(v) => v,
                            Err(_) => { warn!("{} 的密碼太長,跳過", ssid); continue; }
                        };
                        let _ = wifi.set_configuration(&Configuration::Client(ClientConfiguration {
                            ssid: ssid_hs,
                            password: pass_hs,
                            ..Default::default()
                        }));
                        let _ = wifi.start();
                        let _ = wifi.connect();
                        for _ in 0..WIFI_CONNECT_TIMEOUT_SECS {
                            std::thread::sleep(Duration::from_secs(1));
                            feed_watchdog();
                            if wifi.is_connected().unwrap_or(false) {
                                if let Ok(ip_info) = wifi.sta_netif().get_ip_info() {
                                    if !ip_info.ip.is_unspecified() {
                                        // 拿到 IP 不等於通得出去。這裡不驗證的話,會在一條沒有
                                        // 對外能力的網路上宣告「已恢復」,接著那封通知也送不出去。
                                        if !internet_reachable() {
                                            warn!("Health reconnect: {} 有 IP 但無對外連線,換下一個", ssid);
                                            let _ = wifi.disconnect();
                                            break;
                                        }
                                        info!("Health reconnect 成功: {} ({}) [第{}輪]", ssid, ip_info.ip, recon_round);
                                        consecutive_errors = 0;
                                        last_online = uptime_secs();
                                        remember_connection(&mut nvs, ssid, &ip_info.ip.to_string());
                                        // 螢幕原本只在開機畫一次。執行中換了網路或 DHCP 換了 IP,
                                        // 上面顯示的還是舊值 —— 照著螢幕開控制台就是連不上。
                                        let pin_now = nvs_get(&nvs, "ble_pin").unwrap_or_default();
                                        let _ = show_connected_screen(
                                            &lcd_pins, ssid, &ip_info.ip.to_string(), &pin_now);
                                        let _ = send_telegram(&cfg.tg_token, chat_id_num,
                                            &format!("\u{1f4f6} WiFi 已恢復: {} ({}) [重試{}輪]", ssid, ip_info.ip, recon_round));
                                        break 'health_recon;
                                    }
                                }
                            }
                        }
                        warn!("Health reconnect {} 失敗", ssid);
                        let _ = wifi.disconnect();
                    }
                }
                if !wifi.is_connected().unwrap_or(false) {
                    warn!("Health reconnect: 10 輪皆失敗,暫時放棄,{}秒後健康檢查再試", WIFI_HEALTH_CHECK_SECS);
                }
            }
        }

        // Poll Telegram。這個長輪詢是主迴圈的節奏來源 —— 它擋多久,網頁控制台
        // 送出的指令就要等多久才被處理。有人正在用控制台時縮成 1 秒,操作幾乎
        // 立即有反應;閒著沒人用時回到 8 秒,不必要地拉高 Telegram API 呼叫次數。
        let poll_timeout = if portal_active_recently() { 1 } else { 8 };
        match poll_telegram(&cfg.tg_token, last_id + 1, poll_timeout) {
            Ok(updates) => {
                consecutive_errors = 0;
                // 一次成功的 getUpdates 代表 DNS、TCP、TLS、對外路由全都通了 ——
                // 這比問驅動層 is_connected() 可靠得多,拿它當連線看門狗的心跳。
                last_online = uptime_secs();
                for update in updates {
                    last_id = update.update_id;
                    save_last_telegram_update_id(&mut nvs, last_id);
                    if let Some(msg) = update.message {
                        let cid = msg.chat.id;
                        if cid.to_string() != cfg.chat_id {
                            if cfg.chat_id.is_empty() {
                                // 一鍵綁定:控制台按鈕開啟 t.me/<bot>?start=<配對碼>,使用者按 Start
                                // 後 Telegram 送來「/start <配對碼>」。只認這一句、只認私訊、
                                // 而且發話者就是這個對話本人 —— 知道 bot 帳號的路人、群組都綁不走。
                                let expected = format!("/start {}", tg_bind_code());
                                let is_private = msg.chat.kind == "private"
                                    && msg.from.as_ref().map(|u| u.id) == Some(cid);
                                let code_ok = msg.text.as_deref().map(str::trim) == Some(expected.as_str());
                                if !(is_private && code_ok) {
                                    warn!("未綁定狀態收到非配對訊息,忽略: chat={}", cid);
                                    continue;
                                }
                                cfg.chat_id = cid.to_string();
                                chat_id_num = cid;
                                let _ = nvs.set_str("chat_id", &cid.to_string());
                                // 配對碼用過即作廢,下次解綁重來會拿到新的。
                                TG_BIND_CODE.lock().unwrap_or_else(|e| e.into_inner()).clear();
                                info!("Telegram 綁定成功: chat_id={}", cid);
                                let _ = send_telegram(&cfg.tg_token, cid,
                                    "✅ 綁定成功！我是你的 NANA，之後只回覆你一個人。\n傳 /help 看看我會什麼。");
                                continue;
                            } else {
                                warn!("Unauthorized: {}", cid);
                                continue;
                            }
                        }

                        if let Some(voice) = msg.voice {
                            info!("Voice msg (file_id: {})", voice.file_id);
                            handle_voice(&cfg, &mut nvs, &mut state, cid, &voice.file_id);
                            continue;
                        }

                        if let Some(audio) = msg.audio {
                            let mime = audio.mime_type.as_deref().unwrap_or("audio/mpeg");
                            info!("Audio msg (file_id: {}, mime: {})", audio.file_id, mime);
                            handle_audio_upload(&cfg, &mut nvs, &mut state, cid, &audio.file_id, mime);
                            continue;
                        }

                        if let Some(photo) = msg.photo.last() {
                            info!("Photo msg (file_id: {})", photo.file_id);
                            handle_telegram_photo_upload(
                                &cfg,
                                &mut nvs,
                                &mut state,
                                cid,
                                &photo.file_id,
                                "image/jpeg",
                                msg.caption.as_deref(),
                            );
                            continue;
                        }

                        if let Some(doc) = msg.document {
                            let fname = doc.file_name.as_deref().unwrap_or("unknown");
                            info!("Document: {} ({})", fname, doc.file_id);
                            if fname.ends_with(".bin") {
                                handle_ota_document(&cfg, cid, &doc.file_id, fname);
                            } else if let Some(mime) = infer_media_mime(doc.file_name.as_deref(), doc.mime_type.as_deref()) {
                                if mime.starts_with("image/") {
                                    handle_telegram_photo_upload(
                                        &cfg,
                                        &mut nvs,
                                        &mut state,
                                        cid,
                                        &doc.file_id,
                                        &mime,
                                        msg.caption.as_deref(),
                                    );
                                } else if mime.starts_with("audio/") {
                                    handle_audio_upload(
                                        &cfg,
                                        &mut nvs,
                                        &mut state,
                                        cid,
                                        &doc.file_id,
                                        &mime,
                                    );
                                } else {
                                    let _ = send_telegram(&cfg.tg_token, cid,
                                        &format!("Received: {}\nUnsupported media mime type: {}", fname, mime));
                                }
                            } else {
                                let _ = send_telegram(&cfg.tg_token, cid,
                                    &format!("Received: {}\nOnly .bin OTA, image, and audio files are supported.", fname));
                            }
                            continue;
                        }

                        if let Some(text) = msg.text {
                            info!("Text msg received ({} chars)", text.len());
                            handle_text(&cfg, &mut nvs, &mut state, cid, &text);
                        }
                    }
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                let err_msg = format!("{:?}", e);
                state.diag.log("ERR", &format!("Poll fail #{}: {}", consecutive_errors, str_prefix(&err_msg, 100)));
                if consecutive_errors <= 3 {
                    error!("Poll error ({}): {:?}", consecutive_errors, e);
                }
                // DNS recovery: if we get repeated errors, try to reconnect
                // BUG FIX: use >= (not ==) so reconnect fires every DNS_RETRY_MAX errors
                if consecutive_errors >= DNS_RETRY_MAX && consecutive_errors % DNS_RETRY_MAX == 0 {
                    warn!("連線異常：{} 次連續錯誤，檢查 WiFi 狀態", consecutive_errors);
                    state.diag.log("WARN", "WiFi reconnect check triggered");
                    if !wifi.is_connected().unwrap_or(false) {
                        warn!("WiFi 已斷線，嘗試重新連接...");
                        let recon_nets = wifi_nets_from_nvs(&nvs);
                        'recon: for (ssid, pass) in recon_nets.iter() {
                            info!("重連嘗試 {}...", ssid);
                            if wifi.is_started().unwrap_or(false) { let _ = wifi.stop(); }
                            let ssid_hs = match ssid.as_str().try_into() {
                                Ok(v) => v,
                                Err(_) => { warn!("SSID {} 太長,跳過", ssid); continue; }
                            };
                            let pass_hs = match pass.as_str().try_into() {
                                Ok(v) => v,
                                Err(_) => { warn!("{} 的密碼太長,跳過", ssid); continue; }
                            };
                            let _ = wifi.set_configuration(&Configuration::Client(ClientConfiguration {
                                ssid: ssid_hs,
                                password: pass_hs,
                                ..Default::default()
                            }));
                            let _ = wifi.start();
                            let _ = wifi.connect();
                            for _ in 0..WIFI_CONNECT_TIMEOUT_SECS {
                                std::thread::sleep(Duration::from_secs(1));
                                feed_watchdog();
                                if wifi.is_connected().unwrap_or(false) {
                                    if let Ok(ip_info) = wifi.sta_netif().get_ip_info() {
                                        if !ip_info.ip.is_unspecified() {
                                            if !internet_reachable() {
                                                warn!("重連 {} 有 IP 但無對外連線,換下一個", ssid);
                                                let _ = wifi.disconnect();
                                                break;
                                            }
                                            info!("重連成功！SSID={} IP={}", ssid, ip_info.ip);
                                            consecutive_errors = 0;
                                            last_online = uptime_secs();
                                            last_wifi_health_check = now_epoch();
                                            remember_connection(&mut nvs, ssid, &ip_info.ip.to_string());
                                            let pin_now = nvs_get(&nvs, "ble_pin").unwrap_or_default();
                                            let _ = show_connected_screen(
                                                &lcd_pins, ssid, &ip_info.ip.to_string(), &pin_now);
                                            let _ = send_telegram(&cfg.tg_token, chat_id_num,
                                                &format!("\u{1f4f6} WiFi 已恢復連線: {} ({})", ssid, ip_info.ip));
                                            break 'recon;
                                        }
                                    }
                                }
                            }
                            warn!("重連 {} 失敗", ssid);
                        }
                    } else {
                        // Sleep in chunks to avoid WDT
                        for _ in 0..5 {
                            std::thread::sleep(Duration::from_millis(DNS_RETRY_DELAY_MS));
                            feed_watchdog();
                        }
                    }
                }
                let backoff = std::cmp::min(consecutive_errors * 5, 60);
                // Sleep in small chunks to avoid WDT timeout
                for _ in 0..backoff {
                    std::thread::sleep(Duration::from_secs(1));
                    feed_watchdog();
                }
            }
        }

        std::thread::sleep(Duration::from_millis(200));
    }
}

// ===== Voice Handler =====

fn handle_voice(cfg: &Config, nvs: &mut EspNvs<NvsDefault>, state: &mut AppState, chat_id: i64, file_id: &str) {
    let _ = send_telegram(&cfg.tg_token, chat_id, "Transcribing voice...");

    let audio_data = match download_telegram_file(&cfg.tg_token, file_id) {
        Ok(d) => d,
        Err(e) => {
            error!("Download voice: {:?}", e);
            let _ = send_telegram(&cfg.tg_token, chat_id, "Voice download failed");
            return;
        }
    };
    info!("Voice downloaded: {} bytes", audio_data.len());

    let transcript = match transcribe_audio(&cfg.gemini_key, &audio_data, state) {
        Ok(t) => t,
        Err(e) => {
            error!("Transcribe: {:?}", e);
            let _ = send_telegram(&cfg.tg_token, chat_id, "Voice transcription failed");
            return;
        }
    };

    info!("Voice transcribed ({} chars)", transcript.len());

    let reply = match ask_gemini_with_context(&cfg.gemini_key, &transcript, state) {
        Ok(r) => r,
        Err(e) => {
            state.diag.log("ERR", &format!("Voice Gemini: {:?}", e));
            format!("AI Error: {}", e)
        }
    };

    let clean_reply = parse_all_ai_commands(&reply, state, nvs);
    let (clean_reply, _) = (clean_reply, false);
    let (clean_reply, control_notes) = parse_control_tags(&clean_reply, nvs, state);
    let (clean_reply, had_mem) = parse_memory_tags(&clean_reply, nvs, state);
    let (clean_reply, had_pc) = parse_pc_tags(&clean_reply, chat_id);
    let (clean_reply, had_py) = parse_py_tags(&clean_reply, chat_id);
    let (clean_reply, had_img) = parse_img_tags(&clean_reply, chat_id);
    let (clean_reply, had_excel) = parse_excel_tags(&clean_reply, chat_id);
    let (clean_reply, had_cam) = parse_camera_tags(&clean_reply, cfg, nvs, state, chat_id);

    let mut notes = Vec::new();
    for note in control_notes { notes.push(note); }
    if had_mem { notes.push("Memory updated".to_string()); }
    if had_pc { notes.push("PC cmd sent".to_string()); }
    if had_py { notes.push("Python sent".to_string()); }
    if had_img { notes.push("Image gen sent".to_string()); }
    if had_excel { notes.push("Excel sent".to_string()); }
    if had_cam { notes.push("\u{1f4f7} Camera".to_string()); }
    let note_str = if notes.is_empty() {
        String::new()
    } else {
        format!("\n[{}]", notes.join("] ["))
    };
    let user_reply = strip_face_tags(&clean_reply);
    let full_reply = format!(
        "\u{1f3a4} \"{}\"\n\n{}{}{}",
        transcript,
        user_reply,
        note_str,
        token_footer(state)
    );

    if let Err(e) = send_telegram(&cfg.tg_token, chat_id, &full_reply) {
        error!("Reply failed: {:?}", e);
    }

    let lcd_pins = load_lcd_pins(nvs);
    let display_reply = format!("{}{}{}", clean_reply, note_str, token_footer(state));
    let _ = show_reply_screen(&lcd_pins, &display_reply);
    if should_speak_onboard(&state.voice_mode) {
        let speech_text = speech_text_for_mode(&state.voice_mode, &user_reply);
        if let Err(err) = speak_onboard_text(nvs, &speech_text) {
            warn!("Onboard TTS failed: {}", err);
            state.diag.log("WARN", &format!("Voice TTS: {}", err));
        }
    }
}

fn handle_audio_upload(
    cfg: &Config,
    nvs: &mut EspNvs<NvsDefault>,
    state: &mut AppState,
    chat_id: i64,
    file_id: &str,
    mime_type: &str,
) {
    let _ = send_telegram(&cfg.tg_token, chat_id, "Transcribing audio...");

    let audio_data = match download_telegram_file(&cfg.tg_token, file_id) {
        Ok(d) => d,
        Err(e) => {
            error!("Download audio: {:?}", e);
            let _ = send_telegram(&cfg.tg_token, chat_id, "Audio download failed");
            return;
        }
    };
    info!("Audio downloaded: {} bytes ({})", audio_data.len(), mime_type);

    let transcript = match transcribe_audio_with_mime(&cfg.gemini_key, &audio_data, mime_type, state) {
        Ok(t) => t,
        Err(e) => {
            error!("Audio transcribe: {:?}", e);
            let _ = send_telegram(&cfg.tg_token, chat_id, "Audio transcription failed");
            return;
        }
    };

    info!("Audio transcribed ({} chars)", transcript.len());

    let reply = match ask_gemini_with_context(&cfg.gemini_key, &transcript, state) {
        Ok(r) => r,
        Err(e) => {
            state.diag.log("ERR", &format!("Audio Gemini: {:?}", e));
            format!("AI Error: {}", e)
        }
    };

    let clean_reply = parse_all_ai_commands(&reply, state, nvs);
    let (clean_reply, _) = (clean_reply, false);
    let (clean_reply, control_notes) = parse_control_tags(&clean_reply, nvs, state);
    let (clean_reply, had_mem) = parse_memory_tags(&clean_reply, nvs, state);
    let (clean_reply, had_pc) = parse_pc_tags(&clean_reply, chat_id);
    let (clean_reply, had_py) = parse_py_tags(&clean_reply, chat_id);
    let (clean_reply, had_img) = parse_img_tags(&clean_reply, chat_id);
    let (clean_reply, had_excel) = parse_excel_tags(&clean_reply, chat_id);
    let (clean_reply, had_cam) = parse_camera_tags(&clean_reply, cfg, nvs, state, chat_id);

    let mut notes = Vec::new();
    for note in control_notes { notes.push(note); }
    if had_mem { notes.push("Memory updated".to_string()); }
    if had_pc { notes.push("PC cmd sent".to_string()); }
    if had_py { notes.push("Python sent".to_string()); }
    if had_img { notes.push("Image gen sent".to_string()); }
    if had_excel { notes.push("Excel sent".to_string()); }
    if had_cam { notes.push("\u{1f4f7} Camera".to_string()); }
    let note_str = if notes.is_empty() {
        String::new()
    } else {
        format!("\n[{}]", notes.join("] ["))
    };
    let user_reply = strip_face_tags(&clean_reply);
    let full_reply = format!(
        "\u{1f3a7} \"{}\"

{}{}{}",
        transcript,
        user_reply,
        note_str,
        token_footer(state)
    );

    if let Err(e) = send_telegram(&cfg.tg_token, chat_id, &full_reply) {
        error!("Audio reply failed: {:?}", e);
    }

    let lcd_pins = load_lcd_pins(nvs);
    let display_reply = format!("{}{}{}", clean_reply, note_str, token_footer(state));
    let _ = show_reply_screen(&lcd_pins, &display_reply);
    if should_speak_onboard(&state.voice_mode) {
        let speech_text = speech_text_for_mode(&state.voice_mode, &user_reply);
        if let Err(err) = speak_onboard_text(nvs, &speech_text) {
            warn!("Audio TTS failed: {}", err);
            state.diag.log("WARN", &format!("Audio TTS: {}", err));
        }
    }
}

fn handle_telegram_photo_upload(
    cfg: &Config,
    nvs: &mut EspNvs<NvsDefault>,
    state: &mut AppState,
    chat_id: i64,
    file_id: &str,
    mime_type: &str,
    caption: Option<&str>,
) {
    let caption_text = caption.unwrap_or("").trim();
    let prompt = if caption_text.is_empty() {
        "請用繁體中文描述這張使用者傳送的照片，描述重點人物、物件、文字、場景，並給出建議。".to_string()
    } else {
        format!(
            "使用者傳送了一張照片。請以繁體中文回覆他想要知道的：{}",
            caption_text
        )
    };

    let _ = send_telegram(&cfg.tg_token, chat_id, "正在處理你傳來的照片..");

    let image_data = match download_telegram_file(&cfg.tg_token, file_id) {
        Ok(d) => d,
        Err(e) => {
            error!("Download photo: {:?}", e);
            let _ = send_telegram(&cfg.tg_token, chat_id, "圖片下載失敗");
            return;
        }
    };
    info!("Photo downloaded: {} bytes ({})", image_data.len(), mime_type);

    let reply = match ask_gemini_with_image_context(&cfg.gemini_key, &prompt, &image_data, mime_type, state) {
        Ok(r) => r,
        Err(e) => {
            state.diag.log("ERR", &format!("Photo Gemini: {:?}", e));
            let _ = send_telegram(&cfg.tg_token, chat_id, &format!("圖片處理失敗: {}", e));
            return;
        }
    };

    let clean_reply = parse_all_ai_commands(&reply, state, nvs);
    let (clean_reply, _) = (clean_reply, false);
    let (clean_reply, control_notes) = parse_control_tags(&clean_reply, nvs, state);
    let (clean_reply, had_mem) = parse_memory_tags(&clean_reply, nvs, state);
    let (clean_reply, had_pc) = parse_pc_tags(&clean_reply, chat_id);
    let (clean_reply, had_py) = parse_py_tags(&clean_reply, chat_id);
    let (clean_reply, had_img) = parse_img_tags(&clean_reply, chat_id);
    let (clean_reply, had_excel) = parse_excel_tags(&clean_reply, chat_id);
    let (clean_reply, had_cam) = parse_camera_tags(&clean_reply, cfg, nvs, state, chat_id);

    let mut notes = Vec::new();
    for note in control_notes { notes.push(note); }
    if had_mem { notes.push("Memory updated".to_string()); }
    if had_pc { notes.push("PC cmd sent".to_string()); }
    if had_py { notes.push("Python sent to PC".to_string()); }
    if had_img { notes.push("Image gen sent to PC".to_string()); }
    if had_excel { notes.push("Excel pipeline sent to PC".to_string()); }
    if had_cam { notes.push("\u{1f4f7} Camera".to_string()); }
    let note_str = if notes.is_empty() {
        String::new()
    } else {
        format!("\n[{}]", notes.join("] ["))
    };
    let final_reply = format!("📷 {}{}{}", strip_face_tags(&clean_reply), note_str, token_footer(state));

    if let Err(e) = send_telegram(&cfg.tg_token, chat_id, &final_reply) {
        error!("Photo reply failed: {:?}", e);
    }

    let lcd_pins = load_lcd_pins(nvs);
    let display_reply = format!("{}{}{}", clean_reply, note_str, token_footer(state));
    let _ = show_reply_screen(&lcd_pins, &display_reply);
    if should_speak_onboard(&state.voice_mode) {
        let speech_text = speech_text_for_mode(&state.voice_mode, &strip_face_tags(&clean_reply));
        if let Err(err) = speak_onboard_text(nvs, &speech_text) {
            warn!("Photo TTS failed: {}", err);
            state.diag.log("WARN", &format!("Photo TTS: {}", err));
        }
    }
}

// ===== Text Handler =====

fn handle_text(
    cfg: &Config,
    nvs: &mut EspNvs<NvsDefault>,
    state: &mut AppState,
    chat_id: i64,
    text: &str,
) {
    let trimmed = text.trim();
    // 指令不分大小寫：/TASKS = /tasks = /Tasks
    // 只對指令名稱（第一個空白前）做小寫，保留參數原始大小寫（WiFi 密碼等）
    let trimmed_lc = trimmed.to_ascii_lowercase();
    let trimmed_buf: String;
    let trimmed = if trimmed_lc.starts_with('/') {
        if let Some(sp) = trimmed.find(char::is_whitespace) {
            trimmed_buf = format!("{}{}", &trimmed_lc[..sp], &trimmed[sp..]);
            trimmed_buf.as_str()
        } else {
            trimmed_lc.as_str()
        }
    } else { trimmed };

    // ===== HIGHEST-PRIORITY: Emergency commands (always work, even when paused) =====
    match trimmed {
        // 暫停 = 只設旗標,不動任務。主迴圈本來就有 `if !PAUSED` 才跑任務,
        // 旗標本身就足以停住一切;原本這裡還 clear() + 存檔,等於「暫停」實際上是
        // 「永久刪光所有任務」,而 /resume 又還原不回來 —— 名稱與文件都說可恢復,
        // 使用者按下去就再也拿不回自己建的任務。
        "/pause" => {
            PAUSED.store(true, Ordering::Relaxed);
            let count = state.auto_tasks.iter().filter(|t| t.active).count();
            let _ = send_telegram(&cfg.tg_token, chat_id,
                &format!("⏸️ 系統已暫停！\n{} 個自動任務暫停中(沒有刪除)\n所有 AI 呼叫已停止。\n\n傳送 /resume 恢復", count));
            return;
        }
        "/resume" => {
            PAUSED.store(false, Ordering::Relaxed);
            GEMINI_CALL_COUNT.store(0, Ordering::Relaxed);
            let _ = send_telegram(&cfg.tg_token, chat_id, "▶️ 系統已恢復！AI 功能已啟用。");
            return;
        }
        // 同樣不刪任務 —— 訊息末尾就寫著「傳送 /resume 重新啟動」,刪掉就恢復不了。
        // 真的要清空有 /autotask clear。提醒維持停用(那是明確的「停用」語意)。
        "/shutdown" => {
            PAUSED.store(true, Ordering::Relaxed);
            let task_count = state.auto_tasks.iter().filter(|t| t.active).count();
            for r in &mut state.reminders { r.active = false; }
            save_reminders_to_nvs(nvs, state);
            let _ = send_telegram(&cfg.tg_token, chat_id,
                &format!("🔴 系統已關機！\n{} 個自動任務暫停中(沒有刪除)\n已停用所有提醒\n所有 AI 功能已停止。\n\n傳送 /resume 重新啟動", task_count));
            return;
        }
        _ => {}
    }

    // If globally paused, reject everything except the above emergency commands
    if PAUSED.load(Ordering::Relaxed) {
        let _ = send_telegram(&cfg.tg_token, chat_id,
            "⏸️ 系統已暫停。\n傳送 /resume 恢復\n傳送 /shutdown 完全關機");
        return;
    }

    match trimmed {
        "/start" | "/help" => {
            let help = format!(
                "\u{1f916} NANA v{} | {}\n\n\
                 == System ==\n\
                 /status - System info\n\
                 /wifi - WiFi status\n\
                 /wifi set SSID pass (slot 1)\n\
                 /wifi set2~5 SSID pass\n\
                 /wifi del <1~5> / swap / clear\n\
                 /model - Model & capabilities\n\
                 /model set flash|latest|3.8-flash|pro|gemini-<任意名>\n\
                 /voice - Voice reply mode\n\
                 /voice off|normal|brief\n\
                 /briefing - Morning/weather/news brief\n\
                 /email - ESP direct email tool\n\
                 /camera - OV3660 snapshot + Gemini Vision\n\
                 /lcd - ST7789 screen tools\n\
                 /audio - onboard speaker / mic tools\n\
                 /tokens - Token usage\n\
                 /token - Telegram Token 管理\n\
                 /diag - Self-diagnostics\n\n\
                 == Memory ==\n\
                 /memories - View memories\n\
                 /remember key value - Save\n\
                 /forget key - Delete\n\n\
                 == Schedule ==\n\
                 /reminders - List reminders\n\
                 /remind 5m msg - Every N min\n\
                 /remind 8:00 msg - Daily\n\
                 /remind del 3 - Delete #3\n\n\
                 == Skills ==\n\
                 /skills - List installed skills\n\
                 /skill add name|desc|trigger\n\
                 /skill del name\n\n\
                 == AutoTasks ==\n\
                 /tasks - List background tasks\n\
                 /tasks del ID - Stop a task\n\
                 /tasks clear - Clear all tasks\n\
                 /autotask - Natural language setup\n\
                 /stop - Emergency stop all + pause\n\
                 /pause - Pause all AI calls\n\
                 /resume - Resume AI calls\n\
                 /shutdown - Full shutdown\n\n\
                 == PC Control ==\n\
                 /pc <cmd> <args> - PC control\n\
                 /pc_unlock - Allow AI to send PC commands\n\
                 /pc_lock - Block AI PC commands (default)\n\
                 /screenshot - PC desktop screenshot\n\
                 /update - PC rebuild & flash\n\n\
                 == OTA ==\n\
                 /ota <url> - WiFi OTA update\n\
                 Send .bin file -> OTA update\n\
                 /clear - \u{6e05}\u{5c0d}\u{8a71}\u{7d00}\u{9304}\u{ff08}\u{8a2d}\u{5b9a}\u{4fdd}\u{7559}\u{ff09}\n\
                 /reset - \u{6062}\u{5fa9}\u{539f}\u{5ee0}\u{ff08}\u{6703}\u{522a}\u{6389} WiFi \u{8207}\u{91d1}\u{9470}\u{ff09}\n\n\
                 Text/voice -> AI chat",
                FW_VERSION, state.current_model
            );
            let _ = send_telegram(&cfg.tg_token, chat_id, &help);
            return;
        }
        "/status" => {
            let heap = unsafe { esp_idf_svc::sys::esp_get_free_heap_size() };
            let now = now_epoch();
            let uptime = now - state.boot_time;
            let (h, m) = epoch_to_hhmm(now);
            let active = state.reminders.iter().filter(|r| r.active).count();
            let total_tok = state.tokens_in + state.tokens_out;
            let diag_count = state.diag.entries.len();
            let bot_user = fetch_bot_username(&cfg.tg_token).unwrap_or_else(|_| "unknown".into());
            let sr_active = WAKENET.lock().ok().map_or(false, |g| g.is_some());
            let tts_cached = TTS_CACHE.lock().ok().map_or(0, |c| c.len());
            let status = format!(
                "\u{1f916} NANA Status\n\
                 FW: v{} | Model: {}\n\
                 Bot: @{}\n\
                 Voice: {}\n\
                 Wake: {} ({}) | ESP-SR: {}\n\
                 Time: {:02}:{:02}\n\
                 Uptime: {}m\n\
                 RAM: {}KB free\n\
                 Reminders: {} active\n\
                 Memories: {}/{}\n\
                 Skills: {}\n\
                 TTS Cache: {}/{}\n\
                 Tokens: {} total ({} req)\n\
                 History: {} msgs\n\
                 Diag: {} entries",
                FW_VERSION,
                state.current_model,
                bot_user,
                state.voice_mode,
                if state.wake_enabled { "on" } else { "off" },
                state.wake_phrase,
                if sr_active { "active" } else { "off" },
                h, m,
                uptime / 60,
                heap / 1024, active, state.memories.len(), MAX_MEMORIES,
                state.skills.len(),
                tts_cached, TTS_CACHE_MAX,
                total_tok, state.requests,
                state.history.len(), diag_count
            );
            let _ = send_telegram(&cfg.tg_token, chat_id, &status);
            return;
        }
        "/wifi" => {
            handle_wifi_command(cfg, nvs, chat_id, "status");
            return;
        }
        "/model" => {
            let heap = unsafe { esp_idf_svc::sys::esp_get_free_heap_size() };
            // latest 別名要標明它現在大概是哪一版，不然使用者不知道自己在用什麼。
            let latest_note = if state.current_model.ends_with("-latest") {
                match probe_latest_flash(&cfg.gemini_key) {
                    Some(n) => format!("  (Google 自動指向最新；目前最高版 {})", n),
                    None => "  (Google 自動指向最新；版號查詢失敗)".to_string(),
                }
            } else { String::new() };
            let model_info = format!(
                "\u{1f9e0} NANA Model Info\n\n\
                 AI Model: {}{}\n\
                 Default: {}\n\
                 Firmware: v{}\n\
                 Hardware: ESP32-S3 N16R8\n\
                 Flash: 16MB | PSRAM: 8MB\n\
                 Free RAM: {}KB\n\n\
                 == Model Switch ==\n\
                 /model set flash   (= latest, 預設, 自動跟最新)\n\
                 /model set 3.8-flash | 3.7-flash | 3.5-flash (釘死版本)\n\
                 /model set pro | 3.1-pro\n\
                 /model set gemini-<任意名>  (新模型不用等改韌體)\n\n\
                 == Supported ==\n\
                 {}\n\
                 == Capabilities ==\n\
                 \u{2705} AI Chat (Traditional Chinese)\n\
                 \u{2705} Voice Transcription\n\
                 \u{2705} PC Control (full access)\n\
                 \u{2705} Memory System ({}/{})\n\
                 \u{2705} Reminder/Scheduler\n\
                 \u{2705} OTA Self-Update\n\
                 \u{2705} Python Code Execution\n\
                 \u{2705} Image Generation\n\
                 \u{2705} Excel Processing\n\
                 \u{2705} Self-Diagnostics\n\
                 \u{2705} Skill System ({} skills)",
                state.current_model, latest_note, DEFAULT_GEMINI_MODEL, FW_VERSION,
                heap / 1024,
                supported_models_text(),
                state.memories.len(), MAX_MEMORIES,
                state.skills.len()
            );
            let _ = send_telegram(&cfg.tg_token, chat_id, &model_info);
            return;
        }
        "/voice" => {
            let msg = format!(
                "Current voice reply mode: {}\n\nUse:\n/voice off\n/voice normal\n/voice brief\n\n`off` = 不播報\n`normal` = 語音完整播報\n`brief` = 語音簡句播報。",
                state.voice_mode
            );
            let _ = send_telegram(&cfg.tg_token, chat_id, &msg);
            return;
        }
        "/diag" => {
            let report = state.diag.format_report();
            let _ = send_telegram(&cfg.tg_token, chat_id, &report);
            return;
        }
        "/skills" => {
            if state.skills.is_empty() {
                let _ = send_telegram(&cfg.tg_token, chat_id,
                    "No skills installed.\n/skill add name|description|trigger");
            } else {
                let mut s = format!("Skills ({}):\n", state.skills.len());
                for (i, sk) in state.skills.iter().enumerate() {
                    s.push_str(&format!("{}. {} [{}]\n   {}\n",
                        i+1, sk.name, sk.trigger, sk.description));
                }
                s.push_str("\n/skill add name|desc|trigger\n/skill del name");
                let _ = send_telegram(&cfg.tg_token, chat_id, &s);
            }
            return;
        }
        "/tokens" => {
            let _ = send_telegram(&cfg.tg_token, chat_id, &state.token_report());
            return;
        }
        "/reminders" => {
            let _ = send_telegram(&cfg.tg_token, chat_id, &state.list_reminders());
            return;
        }
        "/autotask" => {
            let _ = send_telegram(&cfg.tg_token, chat_id, &list_auto_tasks(state));
            return;
        }
        // 只清當下的對話上下文。原本沒有這個指令,想「讓她忘掉剛剛聊的」只找得到
        // /reset —— 而那個會把 WiFi 與金鑰一起清光。少了這條路,誤用只是時間問題。
        "/clear" => {
            let n = state.history.len();
            state.history.clear();
            let _ = send_telegram(&cfg.tg_token, chat_id,
                &format!("\u{1f9f9} \u{5df2}\u{6e05}\u{7a7a}\u{5c0d}\u{8a71}\u{7d00}\u{9304}\u{ff08}{} \u{7b46}\u{ff09}\u{3002}\n\
                          \u{8a2d}\u{5b9a}\u{3001}\u{8a18}\u{61b6}\u{3001}\u{63d0}\u{9192}\u{3001}\u{6280}\u{80fd}\u{3001}\u{5b9a}\u{6642}\u{4efb}\u{52d9}\u{90fd}\u{4fdd}\u{7559}\u{3002}", n));
            return;
        }
        // 「/reset」聽起來像「重設對話」,實際上是恢復原廠設定 —— 它會刪掉 WiFi 密碼、
        // Gemini 金鑰、Telegram token。這些東西沒有備份:金鑰只存在使用者的 Google
        // 帳號裡,誤刪之後只能請使用者整組重新輸入,裝置在那之前完全不能用。
        //
        // 名字誤導 + 一句話就執行 + 不可復原,這三個湊在一起遲早出事(實測已經出過)。
        // 所以拆成兩段:先講清楚會刪什麼,要再打一次 confirm 才真的動手。
        "/reset" => {
            let _ = send_telegram(&cfg.tg_token, chat_id,
                "\u{26a0}\u{fe0f} /reset \u{662f}\u{300c}\u{6062}\u{5fa9}\u{539f}\u{5ee0}\u{8a2d}\u{5b9a}\u{300d}\u{ff0c}\u{4e0d}\u{662f}\u{6e05}\u{5c0d}\u{8a71}\u{7d00}\u{9304}\u{3002}\n\n\
                 \u{6703}\u{522a}\u{6389}：WiFi \u{5e33}\u{865f}\u{5bc6}\u{78bc}\u{3001}Gemini \u{91d1}\u{9470}\u{3001}\
                 Telegram token\u{3001}\u{8a9e}\u{97f3}\u{8207}\u{559a}\u{9192}\u{8a5e}\u{8a2d}\u{5b9a}\u{3002}\n\
                 \u{4e0d}\u{6703}\u{522a}：\u{8a18}\u{61b6}\u{3001}\u{63d0}\u{9192}\u{3001}\u{6280}\u{80fd}\u{3001}\u{5b9a}\u{6642}\u{4efb}\u{52d9}\u{3002}\n\n\
                 \u{91d1}\u{9470}\u{6c92}\u{6709}\u{5099}\u{4efd}\u{ff0c}\u{522a}\u{4e86}\u{53ea}\u{80fd}\u{91cd}\u{65b0}\u{8f38}\u{5165}\u{3002}\n\
                 \u{78ba}\u{5b9a}\u{8981}\u{6e05}\u{7a7a}\u{5c31}\u{6253}：/reset confirm\n\n\
                 \u{53ea}\u{60f3}\u{6e05}\u{5c0d}\u{8a71}\u{7d00}\u{9304}\u{7684}\u{8a71}\u{7528}：/clear");
            return;
        }
        "/reset confirm" => {
            let _ = send_telegram(&cfg.tg_token, chat_id, "Resetting...");
            for k in &["wifi_ssid", "wifi_pass", "tg_token", "gemini_key", "chat_id", "model_pref", "voice_mode", "wake_phrase", "wake_enabled", "email_api_key", "email_from", "email_to"] {
                let _ = nvs.remove(k);
            }
            unsafe { esp_idf_svc::sys::esp_restart(); }
        }
        "/driver" => {
            let _ = send_telegram(&cfg.tg_token, chat_id,
                "PC Driver Status:\n\
                 Protocol: USB Serial JSON\n\
                 Baud: 115200\n\
                 Commands sent: check driver.log\n\n\
                 Start driver on PC:\n\
                 python ethan_driver.py");
            return;
        }
        "/briefing" => {
            let greeting = build_daily_briefing(state, now_epoch());
            let _ = send_telegram(&cfg.tg_token, chat_id, &greeting);
            return;
        }
        "/update" => {
            let _ = send_telegram(&cfg.tg_token, chat_id,
                "Triggering firmware rebuild...\n\
                 PC driver will build & flash.\n\
                 I'll restart in ~2 min.");
            usb_send_pc_cmd("firmware_update", &serde_json::json!({
                "firmware_dir": "C:\\zc",
                "port": "COM5"
            }), chat_id);
            return;
        }
        "/memories" | "/mem" => {
            let _ = send_telegram(&cfg.tg_token, chat_id, &format_memories(&state.memories));
            return;
        }
        "/dump" => {
            // 把完整現有狀態以 JSON 吐到序列 stdout,給網頁設定工具讀回顯示。
            // WiFi 密碼明文(本機 USB 設定工具,等同路由器後台);API 金鑰遮罩前4後4。
            let dump = build_dump_json(state, nvs, false);
            {
                let _lock = SERIAL_TX_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                println!("[DUMP]{}", dump);
            }
            return;
        }
        _ => {}
    }

    // Skill management commands
    if trimmed.starts_with("/skill ") {
        let arg = &trimmed[7..];
        if let Some(rest) = arg.strip_prefix("add ") {
            let parts: Vec<&str> = rest.splitn(3, '|').collect();
            if parts.len() >= 3 {
                // 後端驗證。網頁那組欄位的檢查擋不住直接打 /api/cmd,而技能的
                // description 會被塞進 AI 的 system prompt —— 不設上限的話,
                // 一則超長技能就能把對話 context 擠掉。
                const SKILL_NAME_MAX: usize = 32;
                const SKILL_DESC_MAX: usize = 200;
                const SKILL_TRIG_MAX: usize = 64;
                let name = parts[0].trim();
                let desc = parts[1].trim();
                let trig = parts[2].trim();

                let reject = if name.is_empty() || trig.is_empty() {
                    // 空名稱的技能之後刪不掉:/skill del 用名稱比對
                    Some("名稱與觸發詞不能空白".to_string())
                } else if name.chars().count() > SKILL_NAME_MAX {
                    Some(format!("名稱請控制在 {} 字以內", SKILL_NAME_MAX))
                } else if desc.chars().count() > SKILL_DESC_MAX {
                    Some(format!("說明請控制在 {} 字以內(會佔用 AI 的對話空間)", SKILL_DESC_MAX))
                } else if trig.chars().count() > SKILL_TRIG_MAX {
                    Some(format!("觸發詞請控制在 {} 字以內", SKILL_TRIG_MAX))
                } else if state.skills.iter().any(|s| s.name == name) {
                    Some(format!("已經有叫「{}」的技能了,請先 /skill del {} 或換個名字", name, name))
                } else {
                    None
                };

                if let Some(why) = reject {
                    let _ = send_telegram(&cfg.tg_token, chat_id, &format!("\u{274c} {}", why));
                } else if state.skills.len() >= MAX_SKILLS {
                    let _ = send_telegram(&cfg.tg_token, chat_id,
                        &format!("Max {} skills reached. Delete one first.", MAX_SKILLS));
                } else {
                    state.skills.push(Skill {
                        name: name.to_string(),
                        description: desc.to_string(),
                        trigger: trig.to_string(),
                    });
                    save_skills_to_nvs(nvs, &state.skills);
                    // 之前刪過的內建技能被加回來 → 從 sk_deleted 拿掉，開機才會維持
                    let mut d = deleted_builtins(nvs);
                    if d.iter().any(|x| x.eq_ignore_ascii_case(name)) {
                        d.retain(|x| !x.eq_ignore_ascii_case(name));
                        set_deleted_builtins(nvs, &d);
                    }
                    let _ = send_telegram(&cfg.tg_token, chat_id,
                        &format!("\u{2705} Skill added: {} (trigger: {})", name, trig));
                }
            } else {
                let _ = send_telegram(&cfg.tg_token, chat_id,
                    "Usage: /skill add name|description|trigger");
            }
        } else if let Some(name) = arg.strip_prefix("del ") {
            let name = name.trim();
            let before = state.skills.len();
            state.skills.retain(|s| s.name != name);
            if state.skills.len() < before {
                save_skills_to_nvs(nvs, &state.skills);
                // 內建技能被刪：記下來，開機補種時跳過，否則下次重開又長回來
                if builtin_skills().iter().any(|b| b.name.eq_ignore_ascii_case(name)) {
                    let mut d = deleted_builtins(nvs);
                    if !d.iter().any(|x| x.eq_ignore_ascii_case(name)) {
                        d.push(name.to_string());
                        set_deleted_builtins(nvs, &d);
                    }
                }
                let _ = send_telegram(&cfg.tg_token, chat_id,
                    &format!("\u{2705} Skill removed: {}", name));
            } else {
                let _ = send_telegram(&cfg.tg_token, chat_id,
                    &format!("Skill '{}' not found", name));
            }
        } else {
            let _ = send_telegram(&cfg.tg_token, chat_id,
                "Usage:\n/skill add name|description|trigger\n/skill del name");
        }
        return;
    }

    if trimmed.starts_with("/model ") {
        let arg = trimmed[7..].trim();
        let target = arg.strip_prefix("set ").unwrap_or(arg).trim();
        if let Some(model) = normalize_model_selection(target) {
            state.current_model = model.to_string();
            save_runtime_prefs(nvs, state);
            let _ = send_telegram(
                &cfg.tg_token,
                chat_id,
                &format!("\u{2705} Model switched to {}", state.current_model),
            );
        } else {
            let _ = send_telegram(
                &cfg.tg_token,
                chat_id,
                &format!("Unknown model: {}\n\nSupported:\n{}", target, supported_models_text()),
            );
        }
        return;
    }

    if trimmed.starts_with("/voice ") {
        let arg = trimmed[7..].trim();
        if let Some(mode) = normalize_voice_mode(arg) {
            state.voice_mode = mode.to_string();
            save_runtime_prefs(nvs, state);
            let _ = send_telegram(
                &cfg.tg_token,
                chat_id,
                &format!("\u{2705} Voice reply mode set to {}", state.voice_mode),
            );
        } else {
            let _ = send_telegram(&cfg.tg_token, chat_id, "Usage: /voice off\n/voice normal\n/voice brief");
        }
        return;
    }

    // 已經連上 WiFi 時,要回到「熱點 + 設定頁」唯一的辦法本來是 /wifi clear —— 但那會
    // 把所有 WiFi 設定砍掉。這個指令只設一次性旗標再重開,設定完整保留。
    // 讀數上報設定。設定前先自我驗證,不要等到任務跑起來才發現網址是錯的 ——
    // 那時失敗是靜默的,使用者要等連續三輪告警才會知道。
    // 判讀的 temperature。低=穩定重複性高,高=比較會換角度想。
    // 預設 auto:平常用 0.4,任務失敗重試時自動往上加。
    if trimmed.starts_with("/ai temp") {
        let arg = trimmed.strip_prefix("/ai temp").unwrap_or("").trim();
        if arg.is_empty() {
            let fixed = AI_TEMP_FIXED_X100.load(Ordering::Relaxed);
            let now_t = current_vision_temp();
            let mode = if fixed > 0 {
                format!("固定 {:.2}", (fixed as f32) / 100.0)
            } else {
                format!("auto（基準 {:.1}，失敗重試時自動調高，目前 {:.1}）",
                        VISION_TEMP_BASE, now_t)
            };
            let _ = send_telegram(&cfg.tg_token, chat_id, &format!(
                "\u{1f321}\u{fe0f} 判讀 temperature：{}\n\n\
                 /ai temp auto   交給系統（建議）\n\
                 /ai temp 0.2    更穩定、每次答案更一致\n\
                 /ai temp 0.7    比較會換角度想，適合難判讀的畫面\n\n\
                 auto 的做法：平常 {:.1}，判讀失敗重試時每次 +{:.1}（上限 {:.1}）。\n\
                 同樣的圖用同樣的參數重跑通常還是同一個結果，換個參數才有機會不一樣。",
                mode, VISION_TEMP_BASE, VISION_TEMP_STEP, VISION_TEMP_MAX));
            return;
        }
        if arg.eq_ignore_ascii_case("auto") {
            let _ = nvs.set_str(AI_TEMP_KEY, "auto");
            AI_TEMP_FIXED_X100.store(0, Ordering::Relaxed);
            reset_vision_temp();
            let _ = send_telegram(&cfg.tg_token, chat_id,
                "\u{2705} temperature 交給系統自動決定（失敗會自動調高再試）");
            return;
        }
        match arg.parse::<f32>() {
            Ok(v) if (0.0..=1.0).contains(&v) => {
                let _ = nvs.set_str(AI_TEMP_KEY, arg);
                AI_TEMP_FIXED_X100.store((v * 100.0) as u32, Ordering::Relaxed);
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!(
                    "\u{2705} temperature 固定為 {:.2}\n\
                     （固定之後失敗重試不會再自動調高，要恢復用 /ai temp auto）", v));
            }
            _ => {
                let _ = send_telegram(&cfg.tg_token, chat_id,
                    "請給 0.0~1.0 之間的數字，或 auto。例：/ai temp 0.3");
            }
        }
        return;
    }

    if trimmed.starts_with("/report") {
        let arg = trimmed.strip_prefix("/report").unwrap_or("").trim();

        if arg.is_empty() {
            let url = nvs_get(nvs, REPORT_URL_KEY).unwrap_or_default();
            let has_token = nvs_get(nvs, REPORT_TOKEN_KEY)
                .map(|t| !t.trim().is_empty()).unwrap_or(false);
            let state_txt = if url.trim().is_empty() {
                "尚未設定（讀數只發 Telegram，不會上傳）".to_string()
            } else {
                format!("{}{}", url, if has_token { "（有帶 token）" } else { "" })
            };
            let _ = send_telegram(&cfg.tg_token, chat_id, &format!(
                "\u{1f4e4} 讀數上報\n目前：{}\n\n\
                 設定：/report set https://你的網址/endpoint\n\
                 帶 token：/report token 你的token\n\
                 附上照片：/report photo on（目前{}）\n\
                 測試連線：/report test\n\
                 關閉上報：/report off\n\n\
                 送出的 JSON 欄位（固定不變，方便你寫接收程式）：\n\
                 device    裝置名稱，多台裝置靠這個分辨\n\
                 task_id   哪一個定期任務\n\
                 task_type camera（每輪都報）/ watch（條件式）\n\
                 ts        Unix 秒\n\
                 prompt    當初設定要看什麼\n\
                 value     抽出的數值，讀不到是空字串\n\
                 text      完整回覆原文\n\
                 hit       watch 專用：條件成立與否\n\
                 ok        這輪判讀本身成功與否\n\n\
                 開啟附照片後，會在 JSON 之後另外送一次 image/jpeg，\n\
                 用 X-Device / X-Task-Id / X-Ts 標頭對回那筆 JSON。\n\
                 接收端不想收照片就不必實作，JSON 流程完全不受影響。",
                state_txt,
                if report_photo_enabled(nvs) { "開啟" } else { "關閉" }));
            return;
        }

        if arg.eq_ignore_ascii_case("off") {
            let _ = nvs.remove(REPORT_URL_KEY);
            let _ = send_telegram(&cfg.tg_token, chat_id, "\u{2705} 已關閉讀數上報");
            return;
        }

        if let Some(u) = arg.strip_prefix("set ") {
            let u = u.trim();
            // 自我驗證:網址明顯不對就當場擋下並說清楚哪裡不對,
            // 不要讓使用者以為設好了、等到三輪失敗才發現。
            let why = if !(u.starts_with("http://") || u.starts_with("https://")) {
                Some("網址要以 http:// 或 https:// 開頭")
            } else if u.len() < 12 || !u[8..].contains('/') {
                Some("看起來不是完整的端點網址（要像 https://你的網域/api/readings）")
            } else if u.contains(' ') {
                Some("網址裡不能有空格")
            } else {
                None
            };
            if let Some(w) = why {
                let _ = send_telegram(&cfg.tg_token, chat_id,
                    &format!("\u{274c} 這個網址我不能用：{}\n你給的是：{}", w, u));
                return;
            }
            let _ = nvs.set_str(REPORT_URL_KEY, u);
            // 明文 http 不擋 —— 區網內對自架伺服器用 http 是合理的,強制 https
            // 只會逼使用者去弄自簽憑證,反而更糟。但一定要講清楚風險:
            // 讀數與 token 會以明文經過網路,同網段的人抓得到。
            let warn = if u.starts_with("http://") {
                "\n\n\u{26a0}\u{fe0f} 這是明文連線（http）。讀數與 token 在網路上不加密，\n\
                 同一個網段的人有機會攔截。裝置在工廠內網、伺服器也在內網時可以接受；\n\
                 只要會經過網際網路，請改用 https。"
            } else {
                "\n\n\u{1f512} 已使用 https，傳輸過程加密。"
            };
            let _ = send_telegram(&cfg.tg_token, chat_id, &format!(
                "\u{2705} 上報網址已設定：{}{}\n\n用 /report test 送一筆測試資料過去確認接得到。",
                u, warn));
            return;
        }

        if let Some(t) = arg.strip_prefix("token ") {
            let _ = nvs.set_str(REPORT_TOKEN_KEY, t.trim());
            let _ = send_telegram(&cfg.tg_token, chat_id,
                "\u{2705} token 已設定（之後上報會帶 Authorization: Bearer）");
            return;
        }

        if let Some(v) = arg.strip_prefix("photo") {
            let v = v.trim().to_ascii_lowercase();
            let on = match v.as_str() {
                "on" | "1" | "開" => Some(true),
                "off" | "0" | "關" => Some(false),
                _ => None,
            };
            match on {
                Some(on) => {
                    let _ = nvs.set_str(REPORT_PHOTO_KEY, if on { "1" } else { "0" });
                    let _ = send_telegram(&cfg.tg_token, chat_id, if on {
                        "\u{2705} 已開啟附照片。\n\n\
                         之後每次判讀,除了原本的 JSON,還會另外送一次 image/jpeg 到同一個網址,\n\
                         並用 X-Device / X-Task-Id / X-Ts 標頭讓你對回那筆 JSON。\n\
                         Telegram 那邊也會同時收到照片。\n\n\
                         這樣才驗得出「它到底讀對沒」—— 只看數字永遠看起來很合理。"
                    } else {
                        "\u{2705} 已關閉附照片,之後只送 JSON 文字。"
                    });
                }
                None => {
                    let _ = send_telegram(&cfg.tg_token, chat_id, "用法：/report photo on 或 /report photo off");
                }
            }
            return;
        }

        if arg.eq_ignore_ascii_case("test") {
            let url = nvs_get(nvs, REPORT_URL_KEY).unwrap_or_default();
            if url.trim().is_empty() {
                let _ = send_telegram(&cfg.tg_token, chat_id,
                    "還沒設定網址。先用 /report set https://你的網址/endpoint");
                return;
            }
            match report_reading(nvs, &ble_device_name(), 0, "test",
                                 "連線測試", "123", "這是一筆測試資料", true, true) {
                Ok(()) => {
                    let _ = send_telegram(&cfg.tg_token, chat_id, &format!(
                        "\u{2705} 測試資料已送達 {}\n\
                         去你的伺服器確認收到的內容是不是這樣：\n\
                         task_type=test, value=\"123\", ok=true", url));
                }
                Err(e) => {
                    let _ = send_telegram(&cfg.tg_token, chat_id, &format!(
                        "\u{274c} 送不過去：{}\n\n\
                         常見原因：網址打錯、伺服器沒開、防火牆擋住、\n\
                         或伺服器只收 https 但你給的是 http。\n\
                         裝置與伺服器要在同一個網路，或伺服器要有對外網址。", e));
                }
            }
            return;
        }

        let _ = send_telegram(&cfg.tg_token, chat_id,
            "用法：/report（看設定）/report set 網址 /report token 值 /report test /report off");
        return;
    }

    if trimmed == "/setup" || trimmed == "/setupmode" {
        let _ = nvs.set_str(FORCE_SETUP_KEY, "1");
        let _ = send_telegram(&cfg.tg_token, chat_id,
            &format!("\u{1f527} 重開機進入設定模式…\n\n\
                      約 30 秒後手機找熱點 **{}** 連上,\n\
                      瀏覽器開 http://10.1.1.1(通常會自動跳出)。\n\
                      螢幕上也會顯示熱點名稱{}與登入 PIN。\n\n\
                      \u{26a0}\u{fe0f} 設定模式下不會回訊息(那時它沒連上網路)。\n\
                      設定存檔後會自動重開機恢復正常。\n\
                      按錯了?直接把電源拔掉再插上就回正常運作,設定不會掉。",
                     ble_device_name(),
                     if SETUP_AP_SECURED { "、熱點密碼(每次都不同)" } else { "" }));
        std::thread::sleep(Duration::from_millis(800)); // 等訊息真的送出去再重開
        unsafe { esp_idf_svc::sys::esp_restart(); }
    }

    if trimmed.starts_with("/wifi") {
        let arg = trimmed.strip_prefix("/wifi").unwrap_or("").trim();
        handle_wifi_command(cfg, nvs, chat_id, arg);
        return;
    }

    if trimmed.starts_with("/token ") || trimmed == "/token" {
        let arg = trimmed.strip_prefix("/token").unwrap_or("").trim();
        handle_token_command(cfg, nvs, chat_id, arg);
        return;
    }

    if trimmed.starts_with("/key ") || trimmed == "/key" {
        let arg = trimmed.strip_prefix("/key").unwrap_or("").trim();
        handle_key_command(cfg, nvs, chat_id, arg);
        return;
    }

    if trimmed == "/ble" {
        let status = if BLE_INITIALIZED.load(Ordering::SeqCst) { "運行中" } else { "未啟動" };
        let _ = send_telegram(&cfg.tg_token, chat_id,
            &format!("\u{1f4f6} BLE 藍牙\n裝置名稱: {}\n狀態: {}\n\n\
                      使用 SerialSoul App 搜尋並連線\n\
                      輸入密碼後可修改：\n\
                      • WiFi / Telegram / Gemini 設定\n\
                      • BLE 連線密碼", ble_device_name(), status));
        return;
    }

    if trimmed.starts_with("/email") {
        let arg = trimmed.strip_prefix("/email").unwrap_or("").trim();
        handle_direct_email(cfg, nvs, chat_id, arg);
        return;
    }

    if trimmed.starts_with("/camera") {
        let arg = trimmed.strip_prefix("/camera").unwrap_or("").trim();
        handle_camera_command(cfg, nvs, state, chat_id, arg);
        return;
    }

    if trimmed.starts_with("/lcd") {
        let arg = trimmed.strip_prefix("/lcd").unwrap_or("").trim();
        handle_lcd_command(cfg, nvs, chat_id, arg);
        return;
    }

    if trimmed.starts_with("/audio") {
        let arg = trimmed.strip_prefix("/audio").unwrap_or("").trim();
        handle_audio_command(cfg, nvs, state, chat_id, arg);
        return;
    }

    // /autotask and /tasks are aliases for the same task management
    if trimmed.starts_with("/autotask") || trimmed.starts_with("/tasks") {
        let arg = if trimmed.starts_with("/autotask") {
            trimmed.strip_prefix("/autotask").unwrap_or("").trim()
        } else {
            trimmed.strip_prefix("/tasks").unwrap_or("").trim()
        };
        if arg == "list" || arg.is_empty() {
            let _ = send_telegram(&cfg.tg_token, chat_id, &list_auto_tasks(state));
        } else if let Some(rest) = arg.strip_prefix("del ") {
            if let Ok(id) = rest.trim().parse::<u32>() {
                let before = state.auto_tasks.len();
                state.auto_tasks.retain(|t| t.id != id);
                if state.auto_tasks.len() < before {
                    save_auto_tasks_to_nvs(nvs, state);
                    let _ = send_telegram(&cfg.tg_token, chat_id, &format!("✅ 任務 #{} 已刪除", id));
                } else {
                    let _ = send_telegram(&cfg.tg_token, chat_id, &format!("找不到任務 #{}", id));
                }
            } else {
                let _ = send_telegram(&cfg.tg_token, chat_id, "格式：/tasks del <ID>");
            }
        } else if arg == "clear" || arg == "all" {
            let count = state.auto_tasks.len();
            state.auto_tasks.clear();
            save_auto_tasks_to_nvs(nvs, state);
            let _ = send_telegram(&cfg.tg_token, chat_id,
                &format!("🗑️ 已清空全部 {} 個自動任務", count));
        } else if let Some(rest) = arg.strip_prefix("add ") {
            // 任務建構器直接建立(不經 AI):
            //   add camera <分鐘> <提示詞...> [speak]
            //   add gpio   <分鐘> <腳位> <0|1>
            //   add alert  <分鐘> <訊息...>
            let head: Vec<&str> = rest.trim().splitn(2, ' ').collect();
            let mut ok = false;
            if head.len() == 2 {
                let action = head[0];
                let body = head[1].trim();
                match action {
                    "gpio" => {
                        let g: Vec<&str> = body.split_whitespace().collect();
                        if g.len() >= 3 {
                            if let (Ok(min), Ok(pin), Ok(st)) = (g[0].parse::<u64>(), g[1].parse::<i32>(), g[2].parse::<u32>()) {
                                if min > 0 {
                                    let id = add_auto_task(state, min, "gpio", &format!("{}:{}", pin, st), false);
                                    save_auto_tasks_to_nvs(nvs, state);
                                    let _ = send_telegram(&cfg.tg_token, chat_id,
                                        &format!("✅ 任務#{}：每 {} 分鐘 GPIO{} = {}", id, min, pin, if st != 0 { "供電" } else { "斷電" }));
                                    ok = true;
                                }
                            }
                        }
                    }
                    "alert" => {
                        let g: Vec<&str> = body.splitn(2, ' ').collect();
                        if g.len() == 2 {
                            if let Ok(min) = g[0].parse::<u64>() {
                                if min > 0 && !g[1].trim().is_empty() {
                                    let id = add_auto_task(state, min, "alert", g[1].trim(), true);
                                    save_auto_tasks_to_nvs(nvs, state);
                                    let _ = send_telegram(&cfg.tg_token, chat_id,
                                        &format!("✅ 任務#{}：每 {} 分鐘 大聲提醒「{}」", id, min, g[1].trim()));
                                    ok = true;
                                }
                            }
                        }
                    }
                    "camera" => {
                        let g: Vec<&str> = body.splitn(2, ' ').collect();
                        if g.len() == 2 {
                            if let Ok(min) = g[0].parse::<u64>() {
                                if min > 0 {
                                    let mut prompt = g[1].trim().to_string();
                                    let speak = prompt == "speak" || prompt.ends_with(" speak");
                                    if speak { prompt = prompt.trim_end_matches("speak").trim().to_string(); }
                                    if prompt.is_empty() { prompt = "請描述畫面".to_string(); }
                                    let id = add_auto_task(state, min, "camera", &prompt, speak);
                                    save_auto_tasks_to_nvs(nvs, state);
                                    let _ = send_telegram(&cfg.tg_token, chat_id,
                                        &format!("✅ 任務#{}：每 {} 分鐘 拍照分析", id, min));
                                    ok = true;
                                }
                            }
                        }
                    }
                    "watch" => {
                        let g: Vec<&str> = body.splitn(2, ' ').collect();
                        if g.len() == 2 {
                            if let Ok(min) = g[0].parse::<u64>() {
                                if min > 0 {
                                    let mut cond = g[1].trim().to_string();
                                    let speak = cond == "speak" || cond.ends_with(" speak");
                                    if speak { cond = cond.trim_end_matches("speak").trim().to_string(); }
                                    if !cond.is_empty() {
                                        let id = add_auto_task(state, min, "watch", &cond, speak);
                                        save_auto_tasks_to_nvs(nvs, state);
                                        let _ = send_telegram(&cfg.tg_token, chat_id, &format!(
                                            "✅ 監看任務#{}：每 {} 分鐘檢查「{}」\n符合才通知你，不符合完全安靜。",
                                            id, min, strip_roi_prefix(&cond).1));
                                        ok = true;
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            if !ok {
                let _ = send_telegram(&cfg.tg_token, chat_id,
                    "格式：\n/autotask add watch <分鐘> <要盯的事> [speak]\n/autotask add camera <分鐘> <提示詞> [speak]\n/autotask add gpio <分鐘> <腳位> <0或1>\n/autotask add alert <分鐘> <訊息>");
            }
        } else {
            let _ = send_telegram(&cfg.tg_token, chat_id,
                "📋 任務管理：\n/tasks — 列出目前任務\n/tasks del ID — 停止並刪除任務\n/tasks clear — 清除全部任務\n/autotask del ID — 刪除任務（同 /tasks del ID）");
        }
        return;
    }

    // /stop — 緊急停止所有自動任務 + 暫停系統
    if trimmed == "/stop" {
        PAUSED.store(true, Ordering::Relaxed);
        let count = state.auto_tasks.iter().filter(|t| t.active).count();
        let _ = send_telegram(&cfg.tg_token, chat_id,
            &format!("🛑 緊急停止！\n{} 個自動任務暫停中(沒有刪除)\n系統已暫停，所有 AI 呼叫已停止。\n\n傳送 /resume 恢復\n真的要刪光請用 /autotask clear", count));
        return;
    }

    // /reminders — 完整提醒管理（支援 del / clear）
    if trimmed.starts_with("/reminders") {
        let arg = trimmed.strip_prefix("/reminders").unwrap_or("").trim();
        if arg.is_empty() || arg == "list" {
            let _ = send_telegram(&cfg.tg_token, chat_id, &state.list_reminders());
        } else if let Some(rest) = arg.strip_prefix("del ") {
            if let Ok(id) = rest.trim().parse::<u32>() {
                if state.remove_reminder(id) {
                    save_reminders_to_nvs(nvs, state);
                    let _ = send_telegram(&cfg.tg_token, chat_id, &format!("✅ 提醒 #{} 已刪除", id));
                } else {
                    let _ = send_telegram(&cfg.tg_token, chat_id, &format!("找不到提醒 #{}", id));
                }
            } else {
                let _ = send_telegram(&cfg.tg_token, chat_id, "格式：/reminders del <ID>");
            }
        } else if arg == "clear" || arg == "all" {
            let count = state.reminders.iter().filter(|r| r.active).count();
            for r in &mut state.reminders { r.active = false; }
            save_reminders_to_nvs(nvs, state);
            let _ = send_telegram(&cfg.tg_token, chat_id, &format!("🗑️ 已清空 {} 個提醒", count));
        } else {
            let _ = send_telegram(&cfg.tg_token, chat_id,
                "📋 提醒管理：\n/reminders — 列出目前提醒\n/reminders del ID — 刪除個別提醒\n/reminders clear — 清除全部提醒");
        }
        return;
    }

    // /sleepwatch on|off ??shortcut for sleep-monitoring autotask
    if trimmed.starts_with("/sleepwatch") {
        let arg = trimmed.strip_prefix("/sleepwatch").unwrap_or("").trim();
        if arg == "on" {
            let id = state.next_autotask_id;
            state.next_autotask_id += 1;
            let now = now_epoch();
            state.auto_tasks.push(AutoTask {
                id,
                interval_secs: 60,
                next_trigger: now + 60,
                action: "camera".to_string(),
                prompt: "請描述畫面中有誰在睡覺或打盹，用繁體中文簡短說明".to_string(),
                speak: true,
                active: true,
                retries_left: AUTOTASK_MAX_RETRIES,
                consecutive_failures: 0,
            });
            save_auto_tasks_to_nvs(nvs, state);
            let _ = send_telegram(&cfg.tg_token, chat_id,
                &format!("✅ 睡眠監視已啟動，任務 #{} 每60秒拍照語音播報\n\n停止指令：\n/sleepwatch off\n/autotask del {}", id, id));
        } else if arg == "off" {
            let before = state.auto_tasks.len();
            state.auto_tasks.retain(|t| !(t.interval_secs == 60 && t.action == "camera" && t.prompt.contains("睡覺")));
            let removed = before - state.auto_tasks.len();
            save_auto_tasks_to_nvs(nvs, state);
            if removed > 0 {
                let _ = send_telegram(&cfg.tg_token, chat_id,
                    &format!("✅ 已停止 {} 個睡眠監視任務", removed));
            } else {
                let _ = send_telegram(&cfg.tg_token, chat_id, "沒有找到正在執行的任務（可能已停止）");
            }
        } else {
            let _ = send_telegram(&cfg.tg_token, chat_id,
                "💤 睡眠監視快速指令：\n/sleepwatch on  — 每分鐘拍照語音播報誰在睡覺\n/sleepwatch off — 停止所有睡眠監視任務");
        }
        return;
    }

    // PC Safe Mode toggle
    if trimmed == "/pc_unlock" {
        PC_UNLOCK_UNTIL.store(uptime_secs().saturating_add(PC_UNLOCK_SECS), Ordering::Relaxed);
        PC_SAFE_MODE.store(false, Ordering::Relaxed);
        let _ = send_telegram(&cfg.tg_token, chat_id,
            "🔓 PC Safe Mode OFF — AI 可自動執行 PC 指令（shell, python, file_write, email 等）\n\n⚠️ 注意：AI 回覆中的 [PC:] [PY:] [IMG:] [EXCEL:] 標籤將會直接執行！\n\n10 分鐘後自動重新鎖定；也可傳送 /pc_lock 立即鎖定");
        return;
    }
    if trimmed == "/pc_lock" {
        PC_SAFE_MODE.store(true, Ordering::Relaxed);
        let _ = send_telegram(&cfg.tg_token, chat_id,
            "🔒 PC Safe Mode ON — AI 只能執行安全的唯讀指令（screenshot, status, file_read 等）\n\n危險指令（shell, python, file_write, email）需手動用 /pc 執行");
        return;
    }

    if trimmed.starts_with("/pc ") {
        handle_pc_command(cfg, state, chat_id, &trimmed[4..]);
        return;
    }

    // /screenshot shortcut ??/pc screenshot
    if trimmed == "/screenshot" || trimmed == "/ss" {
        handle_pc_command(cfg, state, chat_id, "screenshot");
        return;
    }

    // "/remind "(帶空白)不會誤吃 /reminders 或 /remember。
    // 此分支先前從未接上(handle_remind_command 是 dead code),手冊寫的
    // 「/remind 8:00 吃藥」全都無效 — 現在接上。
    if trimmed.starts_with("/remind ") {
        let args = trimmed[8..].trim().to_string();
        handle_remind_command(cfg, state, nvs, chat_id, &args);
        return;
    }

    if trimmed.starts_with("/remember ") {
        let arg = &trimmed[10..];
        if let Some((key, val)) = arg.split_once(' ') {
            add_memory(nvs, state, key.trim(), val.trim());
            let _ = send_telegram(&cfg.tg_token, chat_id,
                &format!("\u{2705} Saved: {} = {}", key.trim(), val.trim()));
        } else {
            let _ = send_telegram(&cfg.tg_token, chat_id,
                "Usage: /remember <key> <value>\nEx: /remember coffee \u{9ed1}\u{5496}\u{5561}\u{4e0d}\u{52a0}\u{7cd6}");
        }
        return;
    }

    if trimmed.starts_with("/forget ") {
        let key = trimmed[8..].trim();
        if remove_memory(nvs, state, key) {
            let _ = send_telegram(&cfg.tg_token, chat_id, &format!("\u{2705} Forgot: {}", key));
        } else {
            let _ = send_telegram(&cfg.tg_token, chat_id, &format!("Memory '{}' not found", key));
        }
        return;
    }

    if trimmed.starts_with("/ota ") {
        let url = trimmed[5..].trim();
        handle_ota_url(cfg, chat_id, url);
        return;
    }

    // Quick-detect: Chinese screenshot request ??direct PC screenshot (skip Gemini roundtrip)
    {
        let lower = trimmed.to_lowercase();
        if (lower.contains("\u{622a}\u{5716}") || lower.contains("screenshot"))
            && (lower.contains("\u{96fb}\u{8166}") || lower.contains("\u{87a2}\u{5e55}") || lower.contains("pc") || lower.contains("desktop") || lower.contains("screen"))
        {
            let _ = send_telegram(&cfg.tg_token, chat_id, "\u{1f4f8} \u{6b63}\u{5728}\u{622a}\u{5716}\u{96fb}\u{8166}\u{756b}\u{9762}...");
            usb_send_pc_cmd("screenshot", &serde_json::json!({}), chat_id);
            return;
        }
    }

    // AI conversation
    let reply = match ask_gemini_with_context(&cfg.gemini_key, trimmed, state) {
        Ok(r) => r,
        Err(e) => {
            error!("Gemini error: {:?}", e);
            state.diag.log("ERR", &format!("Gemini: {:?}", e));
            format!("AI Error: {}", e)
        }
    };

    let clean_reply = parse_all_ai_commands(&reply, state, nvs);
    let (clean_reply, _) = (clean_reply, false);
    let (clean_reply, control_notes) = parse_control_tags(&clean_reply, nvs, state);
    let (clean_reply, had_mem) = parse_memory_tags(&clean_reply, nvs, state);
    let (clean_reply, had_pc) = parse_pc_tags(&clean_reply, chat_id);
    let (clean_reply, had_py) = parse_py_tags(&clean_reply, chat_id);
    let (clean_reply, had_img) = parse_img_tags(&clean_reply, chat_id);
    let (clean_reply, had_excel) = parse_excel_tags(&clean_reply, chat_id);
    let (clean_reply, had_cam) = parse_camera_tags(&clean_reply, cfg, nvs, state, chat_id);

    let mut notes = Vec::new();
    for note in control_notes { notes.push(note); }
    if had_mem { notes.push("Memory updated".to_string()); }
    if had_pc { notes.push("PC cmd sent".to_string()); }
    if had_py { notes.push("Python sent to PC".to_string()); }
    if had_img { notes.push("Image gen sent to PC".to_string()); }
    if had_excel { notes.push("Excel pipeline sent to PC".to_string()); }
    if had_cam { notes.push("\u{1f4f7} Camera".to_string()); }
    let note_str = if notes.is_empty() {
        String::new()
    } else {
        format!("\n[{}]", notes.join("] ["))
    };
    let final_reply = format!("{}{}{}", strip_face_tags(&clean_reply), note_str, token_footer(state));

    if let Err(e) = send_telegram(&cfg.tg_token, chat_id, &final_reply) {
        error!("Reply failed: {:?}", e);
    }

    let lcd_pins = load_lcd_pins(nvs);
    let display_reply = format!("{}{}{}", clean_reply, note_str, token_footer(state));
    let _ = show_reply_screen(&lcd_pins, &display_reply);
    if should_speak_onboard(&state.voice_mode) {
        let speech_text = speech_text_for_mode(&state.voice_mode, &strip_face_tags(&clean_reply));
        if let Err(err) = speak_onboard_text(nvs, &speech_text) {
            warn!("Onboard TTS failed: {}", err);
            state.diag.log("WARN", &format!("TTS: {}", err));
        }
    }
}

fn handle_lcd_command(cfg: &Config, nvs: &mut EspNvs<NvsDefault>, chat_id: i64, args: &str) {
    let trimmed = args.trim();

    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("status") {
        let pins = load_lcd_pins(nvs);
        let msg = format!(
            "ST7789 LCD\n\n{}\n\nCommands:\n/lcd default\n/lcd pins rst=21 dc=47 bl=38 sclk=19 sda=20 cs=45 width=240 height=240\n/lcd test\n/lcd draw 你好 娜娜",
            pins.status_text(),
        );
        let _ = send_telegram(&cfg.tg_token, chat_id, &msg);
        return;
    }

    if trimmed.eq_ignore_ascii_case("default") {
        let pins = board_hw::LcdPins::default();
        match save_lcd_pins(nvs, &pins) {
            Ok(()) => {
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("\u{1f504} LCD pins reset to default\n\n{}", pins.status_text()));
            }
            Err(err) => {
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("LCD default save failed: {}", err));
            }
        }
        return;
    }

    if let Some(rest) = trimmed.strip_prefix("pins ") {
        let mut pins = load_lcd_pins(nvs);
        match parse_lcd_pin_args(rest, &mut pins).and_then(|_| save_lcd_pins(nvs, &pins)) {
            Ok(()) => {
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("\u{2705} LCD pins updated\n\n{}", pins.status_text()));
            }
            Err(err) => {
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("LCD pin update failed: {}", err));
            }
        }
        return;
    }

    if trimmed.eq_ignore_ascii_case("test") {
        let pins = load_lcd_pins(nvs);
        match draw_lcd_scene(&pins, "NANA", "LCD test pattern") {
            Ok(()) => {
                let _ = send_telegram(&cfg.tg_token, chat_id, "\u{1f5bc} LCD test pattern rendered");
            }
            Err(err) => {
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("LCD test failed: {}", err));
            }
        }
        return;
    }

    // 預覽配網畫面 —— 那個畫面平常只有「連不上 WiFi」時才會出現,出貨前想確認
    // 它長怎樣、字有沒有超出邊界,總不能為了看一眼就去把 WiFi 設定弄壞。
    if trimmed.eq_ignore_ascii_case("provision") {
        let pins = load_lcd_pins(nvs);
        let ssid = ble_device_name();
        let pin = nvs_get(nvs, "ble_pin").unwrap_or_else(|| "------".to_string());
        match show_provisioning_screen(&pins, &ssid, "10.1.1.1", &pin) {
            Ok(()) => {
                let _ = send_telegram(&cfg.tg_token, chat_id,
                    &format!("\u{1f4fa} 配網畫面預覽\n熱點 {}\n網址 10.1.1.1\nPIN {}", ssid, pin));
            }
            Err(err) => {
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("配網畫面繪製失敗: {}", err));
            }
        }
        return;
    }

    if let Some(rest) = trimmed.strip_prefix("draw ") {
        let pins = load_lcd_pins(nvs);
        match draw_lcd_scene(&pins, "NANA", rest.trim()) {
            Ok(()) => {
                let _ = send_telegram(&cfg.tg_token, chat_id, "\u{1f5bc} LCD message rendered");
            }
            Err(err) => {
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("LCD draw failed: {}", err));
            }
        }
        return;
    }

    let _ = send_telegram(&cfg.tg_token, chat_id, "Usage:\n/lcd\n/lcd default\n/lcd pins rst=21 dc=47 bl=38 sclk=19 sda=20 cs=45 width=240 height=240\n/lcd test\n/lcd draw 你好 娜娜");
}

/// Split "SSID PASSWORD" or "SSID|PASSWORD" into (ssid, password).
/// Supports both space and pipe separators. Leading/trailing whitespace is trimmed.
fn split_ssid_pass(input: &str) -> (String, String) {
    let input = input.trim();
    // Try pipe separator first (backward compat)
    if let Some(pos) = input.find('|') {
        let ssid = input[..pos].trim().to_string();
        let pass = input[pos + 1..].trim().to_string();
        return (ssid, pass);
    }
    // Otherwise split on first space
    if let Some(pos) = input.find(' ') {
        let ssid = input[..pos].trim().to_string();
        let pass = input[pos + 1..].trim().to_string();
        (ssid, pass)
    } else {
        // No separator — SSID only, no password (open network)
        (input.to_string(), String::new())
    }
}

fn wifi_nvs_keys(slot: u8) -> (String, String) {
    if slot == 1 {
        ("wifi_ssid".to_string(), "wifi_pass".to_string())
    } else {
        (format!("wifi_ssid{}", slot), format!("wifi_pass{}", slot))
    }
}

fn handle_wifi_command(cfg: &Config, nvs: &mut EspNvs<NvsDefault>, chat_id: i64, args: &str) {
    let trimmed = args.trim();
    let trimmed_lower = trimmed.to_ascii_lowercase();

    // /wifi (status) — 顯示所有已設定的 WiFi
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("status") {
        let mut lines = vec!["\u{1f4f6} WiFi \u{8a2d}\u{5b9a}\n".to_string()];
        for i in 1..=5u8 {
            let (sk, pk) = wifi_nvs_keys(i);
            let ssid = nvs_get(nvs, &sk).unwrap_or_default();
            let has_pass = nvs_get(nvs, &pk).map(|v| !v.is_empty()).unwrap_or(false);
            if ssid.is_empty() {
                lines.push(format!("  {} (\u{672a}\u{8a2d}\u{5b9a})", i));
            } else {
                let lock = if has_pass { " \u{1f512}" } else { "" };
                lines.push(format!("  {} {}{}", i, ssid, lock));
            }
        }
        lines.push(String::new());
        lines.push("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}".to_string());
        lines.push("\u{6307}\u{4ee4}\u{ff1a}".to_string());
        lines.push("/wifi set<N> SSID \u{5bc6}\u{78bc}".to_string());
        lines.push("  N=1~5  \u{4f8b}: /wifi set SSID \u{5bc6}\u{78bc}".to_string());
        lines.push("  \u{4f8b}: /wifi set3 SSID \u{5bc6}\u{78bc}".to_string());
        lines.push("/wifi del <N>".to_string());
        lines.push("/wifi swap".to_string());
        lines.push("/wifi clear".to_string());
        lines.push(String::new());
        lines.push("\u{1f4a1} \u{8a2d}\u{5b9a}\u{5f8c}\u{7cfb}\u{7d71}\u{81ea}\u{52d5}\u{91cd}\u{555f}\u{ff01}".to_string());
        let _ = send_telegram(&cfg.tg_token, chat_id, &lines.join("\n"));
        return;
    }

    // /wifi clear — 清空所有 WiFi 設定
    if trimmed.eq_ignore_ascii_case("clear") {
        for i in 1..=5u8 {
            let (sk, pk) = wifi_nvs_keys(i);
            let _ = nvs.remove(&sk);
            let _ = nvs.remove(&pk);
        }
        let _ = send_telegram(&cfg.tg_token, chat_id, "\u{2705} WiFi \u{8a2d}\u{5b9a}\u{5df2}\u{5168}\u{90e8}\u{6e05}\u{7a7a}\u{ff0c}\u{7cfb}\u{7d71}\u{5c07}\u{91cd}\u{555f}\u{3002}");
        unsafe { esp_idf_svc::sys::esp_restart(); }
    }

    // /wifi swap — 1↔2 互換
    if trimmed.eq_ignore_ascii_case("swap") {
        let s1 = nvs_get(nvs, "wifi_ssid").unwrap_or_default();
        let p1 = nvs_get(nvs, "wifi_pass").unwrap_or_default();
        let s2 = nvs_get(nvs, "wifi_ssid2").unwrap_or_default();
        let p2 = nvs_get(nvs, "wifi_pass2").unwrap_or_default();
        if s2.is_empty() {
            let _ = send_telegram(&cfg.tg_token, chat_id, "\u{274c} WiFi 2 \u{672a}\u{8a2d}\u{5b9a}\u{ff0c}\u{7121}\u{6cd5}\u{4e92}\u{63db}");
            return;
        }
        let _ = nvs.set_str("wifi_ssid", &s2);
        let _ = nvs.set_str("wifi_pass", &p2);
        let _ = nvs.set_str("wifi_ssid2", &s1);
        let _ = nvs.set_str("wifi_pass2", &p1);
        let _ = send_telegram(&cfg.tg_token, chat_id,
            &format!("\u{2705} \u{4e92}\u{63db}\u{5b8c}\u{6210}\u{ff01}\n1: {} \u{2192} {}\n\u{7cfb}\u{7d71}\u{91cd}\u{555f}\u{4e2d}...", s1, s2));
        unsafe { esp_idf_svc::sys::esp_restart(); }
    }

    // /wifi del <N> — 刪除指定插槽
    if trimmed_lower.starts_with("del ") || trimmed_lower.starts_with("del\t") {
        let rest = trimmed[4..].trim();
        if let Ok(slot) = rest.parse::<u8>() {
            if slot >= 1 && slot <= 5 {
                let (sk, pk) = wifi_nvs_keys(slot);
                let _ = nvs.remove(&sk);
                let _ = nvs.remove(&pk);
                let _ = send_telegram(&cfg.tg_token, chat_id,
                    &format!("\u{2705} WiFi {} \u{5df2}\u{522a}\u{9664}", slot));
                return;
            }
        }
        let _ = send_telegram(&cfg.tg_token, chat_id, "\u{274c} \u{7528}\u{6cd5}\u{ff1a}/wifi del <1~5>");
        return;
    }

    // /wifi set<N> SSID PASSWORD — 設定指定插槽 (set = set1, set2..set5)
    // Parse slot number from "set", "set1", "set2" ... "set5"
    let maybe_set = if trimmed_lower.starts_with("set") {
        let after_set = &trimmed_lower[3..];
        // "set SSID pass" → slot 1, rest starts after "set "
        // "set2 SSID pass" → slot 2, rest starts after "set2 "
        if after_set.starts_with(' ') || after_set.starts_with('\t') {
            Some((1u8, &trimmed[4..]))  // "set " = 4 chars
        } else if after_set.len() >= 2 {
            let digit = after_set.as_bytes()[0];
            let sep = after_set.as_bytes()[1];
            if digit >= b'1' && digit <= b'5' && (sep == b' ' || sep == b'\t') {
                Some((digit - b'0', &trimmed[5..]))  // "setN " = 5 chars
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    if let Some((slot, rest)) = maybe_set {
        let (ssid, password) = split_ssid_pass(rest);
        if ssid.is_empty() {
            let _ = send_telegram(&cfg.tg_token, chat_id,
                &format!("\u{274c} \u{7528}\u{6cd5}\u{ff1a}/wifi set{} SSID \u{5bc6}\u{78bc}", if slot == 1 { String::new() } else { slot.to_string() }));
            return;
        }
        let (sk, pk) = wifi_nvs_keys(slot);
        if nvs.set_str(&sk, &ssid).is_ok() && nvs.set_str(&pk, &password).is_ok() {
            let restart = slot == 1;
            let _ = send_telegram(&cfg.tg_token, chat_id,
                &format!("\u{2705} WiFi {} \u{5df2}\u{8a2d}\u{70ba} {}{}", slot, ssid,
                    if restart { "\n\u{7cfb}\u{7d71}\u{91cd}\u{555f}\u{4e2d}..." } else { "" }));
            if restart {
                unsafe { esp_idf_svc::sys::esp_restart(); }
            }
        } else {
            let _ = send_telegram(&cfg.tg_token, chat_id, "\u{274c} WiFi \u{5beb}\u{5165} NVS \u{5931}\u{6557}");
        }
        return;
    }

    let _ = send_telegram(&cfg.tg_token, chat_id,
        "\u{1f4f6} WiFi \u{6307}\u{4ee4}\u{ff1a}\n\n\
         /wifi \u{2014} \u{67e5}\u{770b}\u{76ee}\u{524d}\u{8a2d}\u{5b9a}\n\
         /wifi set SSID \u{5bc6}\u{78bc} (\u{4e3b}\u{7db2}\u{8def})\n\
         /wifi set2~5 SSID \u{5bc6}\u{78bc}\n\
         /wifi del <1~5>\n\
         /wifi swap \u{2014} 1\u{2194}2\u{4e92}\u{63db}\n\
         /wifi clear \u{2014} \u{6e05}\u{7a7a}\u{5168}\u{90e8}");
}

fn handle_token_command(cfg: &Config, nvs: &mut EspNvs<NvsDefault>, chat_id: i64, args: &str) {
    let trimmed = args.trim();

    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("status") {
        let current = nvs_get(nvs, "tg_token").unwrap_or_default();
        let masked = if current.len() > 10 {
            format!("{}...{}", str_prefix(&current, 5), str_suffix(&current, 5))
        } else {
            "***".to_string()
        };
        let _ = send_telegram(&cfg.tg_token, chat_id,
            &format!("\u{1f511} Telegram Token\n\n\
                      \u{76ee}\u{524d}: {}\n\n\
                      \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\
                      /token set <\u{65b0}TOKEN> \u{2014} \u{66f4}\u{63db} Token\n\n\
                      \u{26a0}\u{fe0f} \u{66f4}\u{63db}\u{5f8c}\u{7cfb}\u{7d71}\u{81ea}\u{52d5}\u{91cd}\u{555f}\u{ff01}", masked));
        return;
    }

    if let Some(rest) = trimmed.strip_prefix("set ").or_else(|| trimmed.strip_prefix("set\t")) {
        let new_token = rest.trim();
        if new_token.is_empty() || new_token.len() < 20 {
            let _ = send_telegram(&cfg.tg_token, chat_id,
                "\u{274c} Token \u{683c}\u{5f0f}\u{4e0d}\u{6b63}\u{78ba}\u{ff0c}\u{61c9}\u{70ba} BotFather \u{63d0}\u{4f9b}\u{7684}\u{5b8c}\u{6574} Token");
            return;
        }
        if nvs.set_str("tg_token", new_token).is_ok() {
            let _ = send_telegram(&cfg.tg_token, chat_id,
                "\u{2705} Telegram Token \u{5df2}\u{66f4}\u{65b0}\u{ff0c}\u{7cfb}\u{7d71}\u{91cd}\u{555f}\u{4e2d}...");
            unsafe { esp_idf_svc::sys::esp_restart(); }
        } else {
            let _ = send_telegram(&cfg.tg_token, chat_id,
                "\u{274c} Token \u{5beb}\u{5165} NVS \u{5931}\u{6557}");
        }
        return;
    }

    let _ = send_telegram(&cfg.tg_token, chat_id,
        "\u{1f511} Token \u{6307}\u{4ee4}\u{ff1a}\n\n\
         /token \u{2014} \u{67e5}\u{770b}\u{76ee}\u{524d} Token\n\
         /token set <TOKEN> \u{2014} \u{66f4}\u{63db} Telegram Bot Token\n\n\
         \u{26a0}\u{fe0f} \u{66f4}\u{63db}\u{5f8c}\u{7cfb}\u{7d71}\u{81ea}\u{52d5}\u{91cd}\u{555f}\u{ff01}");
}

fn handle_key_command(cfg: &Config, nvs: &mut EspNvs<NvsDefault>, chat_id: i64, args: &str) {
    let trimmed = args.trim();

    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("status") {
        let current = nvs_get(nvs, "gemini_key").unwrap_or_default();
        let masked = if current.len() > 10 {
            format!("{}...{}", str_prefix(&current, 5), str_suffix(&current, 3))
        } else {
            "***".to_string()
        };
        let _ = send_telegram(&cfg.tg_token, chat_id,
            &format!("\u{1f511} Gemini API Key\n\n\
                      \u{76ee}\u{524d}: {}\n\n\
                      /key set <KEY> \u{2014} \u{66f4}\u{63db} Gemini API Key\n\n\
                      \u{26a0}\u{fe0f} \u{66f4}\u{63db}\u{5f8c}\u{7cfb}\u{7d71}\u{81ea}\u{52d5}\u{91cd}\u{555f}\u{ff01}", masked));
        return;
    }

    if let Some(rest) = trimmed.strip_prefix("set ").or_else(|| trimmed.strip_prefix("set\t")) {
        let new_key = rest.trim();
        if new_key.is_empty() || !new_key.starts_with("AIza") {
            let _ = send_telegram(&cfg.tg_token, chat_id,
                "\u{274c} Key \u{683c}\u{5f0f}\u{4e0d}\u{6b63}\u{78ba}\u{ff0c}\u{61c9}\u{70ba} AIza... \u{958b}\u{982d}\u{7684} Gemini API Key");
            return;
        }
        if nvs.set_str("gemini_key", new_key).is_ok() {
            let _ = send_telegram(&cfg.tg_token, chat_id,
                "\u{2705} Gemini API Key \u{5df2}\u{66f4}\u{65b0}\u{ff0c}\u{7cfb}\u{7d71}\u{91cd}\u{555f}\u{4e2d}...");
            unsafe { esp_idf_svc::sys::esp_restart(); }
        } else {
            let _ = send_telegram(&cfg.tg_token, chat_id,
                "\u{274c} Key \u{5beb}\u{5165} NVS \u{5931}\u{6557}");
        }
        return;
    }

    let _ = send_telegram(&cfg.tg_token, chat_id,
        "\u{1f511} Gemini Key \u{6307}\u{4ee4}\u{ff1a}\n\n\
         /key \u{2014} \u{67e5}\u{770b}\u{76ee}\u{524d} Key\n\
         /key set <KEY> \u{2014} \u{66f4}\u{63db} Gemini API Key\n\n\
         \u{26a0}\u{fe0f} \u{66f4}\u{63db}\u{5f8c}\u{7cfb}\u{7d71}\u{81ea}\u{52d5}\u{91cd}\u{555f}\u{ff01}");
}

fn handle_audio_command(cfg: &Config, nvs: &mut EspNvs<NvsDefault>, state: &mut AppState, chat_id: i64, args: &str) {
    let trimmed = args.trim();

    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("status") {
        let pins = load_audio_pins(nvs);
        let proxy = load_tts_proxy_url(nvs).unwrap_or_else(|| "disabled".to_string());
        let voice = load_tts_proxy_voice(nvs);
        let msg = format!(
            "Onboard Audio\n\n{}\nWake: {} ({})\nTTS proxy: {}\nTTS voice: {}\n\nCommands:\n/audio default\n/audio pins bclk=40 ws=41 dout=39 mic_ws=1 mic_sck=2 mic_din=42 mclk=-1 rate=24000\n/audio proxy\n/audio proxy set <url>\n/audio proxy off\n/audio proxy voice <name>\n/audio tone\n/audio mic\n/audio transcribe\n/audio say 你好 娜娜\n/audio wake\n/audio wake on|off\n/audio wake set nana\n/audio wake now\n/audio level",
            pins.status_text(),
            if state.wake_enabled { "on" } else { "off" },
            state.wake_phrase,
            proxy,
            voice,
        );
        let _ = send_telegram(&cfg.tg_token, chat_id, &msg);
        return;
    }

    if trimmed.eq_ignore_ascii_case("proxy") {
        let proxy = load_tts_proxy_url(nvs).unwrap_or_else(|| "disabled".to_string());
        let voice = load_tts_proxy_voice(nvs);
        let _ = send_telegram(&cfg.tg_token, chat_id, &format!("TTS proxy: {}\nVoice: {}", proxy, voice));
        return;
    }

    if let Some(rest) = trimmed.strip_prefix("proxy ") {
        let arg = rest.trim();
        if arg.eq_ignore_ascii_case("off") {
            let _ = nvs.remove(TTS_PROXY_URL_KEY);
            let _ = send_telegram(&cfg.tg_token, chat_id, "\u{1f507} TTS proxy disabled");
            return;
        }
        if let Some(voice) = arg.strip_prefix("voice ") {
            let voice = voice.trim();
            if voice.is_empty() {
                let _ = send_telegram(&cfg.tg_token, chat_id, "Usage: /audio proxy voice <voice-name>");
            } else if nvs.set_str(TTS_PROXY_VOICE_KEY, voice).is_ok() {
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("\u{2705} TTS proxy voice set to {}", voice));
            } else {
                let _ = send_telegram(&cfg.tg_token, chat_id, "TTS proxy voice save failed");
            }
            return;
        }
        if let Some(url) = arg.strip_prefix("set ") {
            let url = url.trim();
            if url.is_empty() {
                let _ = send_telegram(&cfg.tg_token, chat_id, "Usage: /audio proxy set <url>");
            } else if nvs.set_str(TTS_PROXY_URL_KEY, url).is_ok() {
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("\u{2705} TTS proxy set to {}", url));
            } else {
                let _ = send_telegram(&cfg.tg_token, chat_id, "TTS proxy save failed");
            }
            return;
        }
    }

    if trimmed.eq_ignore_ascii_case("default") {
        let pins = board_hw::AudioPins::default();
        match save_audio_pins(nvs, &pins) {
            Ok(()) => {
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("\u{1f504} Audio pins reset to default\n\n{}", pins.status_text()));
            }
            Err(err) => {
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("Audio default save failed: {}", err));
            }
        }
        return;
    }

    if let Some(rest) = trimmed.strip_prefix("pins ") {
        let mut pins = load_audio_pins(nvs);
        match parse_audio_pin_args(rest, &mut pins).and_then(|_| save_audio_pins(nvs, &pins)) {
            Ok(()) => {
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("\u{2705} Audio pins updated\n\n{}", pins.status_text()));
            }
            Err(err) => {
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("Audio pin update failed: {}", err));
            }
        }
        return;
    }

    if trimmed.eq_ignore_ascii_case("tone") {
        let pins = load_audio_pins(nvs);
        match play_test_tone(&pins) {
            Ok(()) => {
                let _ = send_telegram(&cfg.tg_token, chat_id, "\u{1f50a} Audio tone test played");
            }
            Err(err) => {
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("Audio tone test failed: {}", err));
            }
        }
        return;
    }

    if trimmed.eq_ignore_ascii_case("mic") {
        let pins = load_audio_pins(nvs);
        match board_hw::capture_mic_snapshot(&pins, 1500) {
            Ok(snapshot) => {
                let _ = send_telegram(
                    &cfg.tg_token,
                    chat_id,
                    &format!(
                        "\u{1f399}\u{fe0f} Mic snapshot\nRMS: {}\nPeak: {}\nSamples: {}",
                        snapshot.rms, snapshot.peak, snapshot.samples
                    ),
                );
            }
            Err(err) => {
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("Mic test failed: {}", err));
            }
        }
        return;
    }

    if trimmed.eq_ignore_ascii_case("transcribe") {
        match capture_and_transcribe_mic(cfg, nvs, state, 2500) {
            Ok(transcript) => {
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("🎤 Mic transcript\n{}", transcript));
            }
            Err(err) => {
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("Mic transcribe failed: {}", err));
            }
        }
        return;
    }

    if let Some(rest) = trimmed.strip_prefix("say ") {
        match speak_onboard_text(nvs, rest.trim()) {
            Ok(speech) => {
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("🔊 Board spoke: {}", speech));
            }
            Err(err) => {
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("Board TTS failed: {}", err));
            }
        }
        return;
    }

    if trimmed.eq_ignore_ascii_case("wake") {
        let _ = send_telegram(
            &cfg.tg_token,
            chat_id,
            &format!("Wake listener: {}\nPhrase: {}\n\nUse:\n/audio wake on\n/audio wake off\n/audio wake set nana\n/audio wake now", if state.wake_enabled { "on" } else { "off" }, state.wake_phrase),
        );
        return;
    }

    if let Some(rest) = trimmed.strip_prefix("wake ") {
        let arg = rest.trim();
        if arg.eq_ignore_ascii_case("on") {
            state.wake_enabled = true;
            save_runtime_prefs(nvs, state);
            let _ = send_telegram(&cfg.tg_token, chat_id, &format!("\u{1f3a4} Wake listener enabled ({})", state.wake_phrase));
            return;
        }
        if arg.eq_ignore_ascii_case("off") {
            state.wake_enabled = false;
            save_runtime_prefs(nvs, state);
            let _ = send_telegram(&cfg.tg_token, chat_id, "\u{1f507} Wake listener disabled");
            return;
        }
        if let Some(phrase) = arg.strip_prefix("set ") {
            let phrase = phrase.trim();
            if phrase.is_empty() {
                let _ = send_telegram(&cfg.tg_token, chat_id, "Usage: /audio wake set <phrase>");
            } else {
                state.wake_phrase = phrase.to_string();
                save_runtime_prefs(nvs, state);
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("\u{2705} Wake phrase set to {}", state.wake_phrase));
            }
            return;
        }
        if arg.eq_ignore_ascii_case("now") {
            match detect_wake_phrase(cfg, nvs, state, 2500) {
                Ok(Some(transcript)) => {
                    let _ = send_telegram(&cfg.tg_token, chat_id, &format!("\u{2705} Wake phrase matched\n{}", transcript));
                    if should_speak_onboard(&state.voice_mode) {
                        let _ = speak_onboard_text(nvs, "好的，請說！");
                    }
                }
                Ok(None) => {
                    let _ = send_telegram(&cfg.tg_token, chat_id, "⏰ Wake phrase not heard this round");
                }
                Err(err) => {
                    let _ = send_telegram(&cfg.tg_token, chat_id, &format!("Wake phrase test failed: {}", err));
                }
            }
            return;
        }
    }

    if trimmed.eq_ignore_ascii_case("level") {
        let pins = load_audio_pins(nvs);
        match wake_probe(&pins, 3000, 3500) {
            Ok(triggered) => {
                let msg = if triggered {
                    "\u{1f50a} Audio level probe triggered: detected voice-level audio"
                } else {
                    "\u{1f50a} Audio level probe idle: no voice-level audio detected"
                };
                let _ = send_telegram(&cfg.tg_token, chat_id, msg);
            }
            Err(err) => {
                let _ = send_telegram(&cfg.tg_token, chat_id, &format!("Audio level probe failed: {}", err));
            }
        }
        return;
    }

    let _ = send_telegram(&cfg.tg_token, chat_id, "Usage:\n/audio\n/audio default\n/audio pins bclk=40 ws=41 dout=39 mic_ws=1 mic_sck=2 mic_din=42 mclk=-1 rate=24000\n/audio proxy\n/audio proxy set <url>\n/audio proxy off\n/audio proxy voice <name>\n/audio tone\n/audio mic\n/audio transcribe\n/audio say 你好 娜娜\n/audio wake\n/audio wake on|off\n/audio wake set nana\n/audio wake now\n/audio level");
}

// ===== PC Command Handler =====

fn handle_pc_command(cfg: &Config, _state: &mut AppState, chat_id: i64, args: &str) {
    let parts: Vec<&str> = args.splitn(2, ' ').collect();
    if parts.is_empty() || parts[0].is_empty() {
        let _ = send_telegram(&cfg.tg_token, chat_id,
            "/pc commands:\n\
             shell <cmd> - run command\n\
             file_read <path>\n\
             file_list [path]\n\
             file_write <path> <content>\n\
             screenshot\n\
             status\n\
             open <target>\n\
             click <x> <y>\n\
             type <text>\n\
             hotkey <key1+key2>\n\
             mouse_move <x> <y>\n\
             scroll <amount>\n\
             find_window <title>\n\
             focus_window <title>\n\
             excel_read <path>\n\
             email_send <to> <subj> <body>");
        return;
    }

    let cmd = parts[0];
    let rest = if parts.len() > 1 { parts[1] } else { "" };

    let (driver_cmd, args_json) = match cmd {
        "shell" => ("shell", serde_json::json!({"command": rest})),
        "file_read" => ("file_read", serde_json::json!({"path": rest})),
        "file_list" => ("file_list", serde_json::json!({"path": if rest.is_empty() { "." } else { rest }})),
        "file_write" => {
            let sub: Vec<&str> = rest.splitn(2, ' ').collect();
            ("file_write", serde_json::json!({"path": sub.first().unwrap_or(&""), "content": sub.get(1).unwrap_or(&"")}))
        }
        "screenshot" => ("screenshot", serde_json::json!({})),
        "status" => ("status", serde_json::json!({})),
        "open" => ("open", serde_json::json!({"target": rest})),
        "click" => {
            let sub: Vec<&str> = rest.splitn(2, ' ').collect();
            let x: i32 = sub.first().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            let y: i32 = sub.get(1).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            ("click", serde_json::json!({"x": x, "y": y, "button": "left"}))
        }
        "type" => ("type", serde_json::json!({"text": rest})),
        "hotkey" => {
            let keys: Vec<&str> = rest.split('+').map(|s| s.trim()).collect();
            ("hotkey", serde_json::json!({"keys": keys}))
        }
        "mouse_move" => {
            let sub: Vec<&str> = rest.splitn(2, ' ').collect();
            let x: i32 = sub.first().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            let y: i32 = sub.get(1).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            ("mouse_move", serde_json::json!({"x": x, "y": y}))
        }
        "scroll" => {
            let amount: i32 = rest.trim().parse().unwrap_or(3);
            ("scroll", serde_json::json!({"amount": amount}))
        }
        "find_window" => ("find_window", serde_json::json!({"title": rest})),
        "focus_window" => ("focus_window", serde_json::json!({"title": rest})),
        "excel_read" => ("excel", serde_json::json!({"action": "read", "path": rest})),
        "excel_write" => {
            let sub: Vec<&str> = rest.splitn(3, ' ').collect();
            ("excel", serde_json::json!({"action": "write", "path": sub.first().unwrap_or(&""), "cell": sub.get(1).unwrap_or(&"A1"), "value": sub.get(2).unwrap_or(&"")}))
        }
        "email_send" => {
            let sub: Vec<&str> = rest.splitn(3, ' ').collect();
            ("email", serde_json::json!({"action": "send", "to": sub.first().unwrap_or(&""), "subject": sub.get(1).unwrap_or(&""), "body": sub.get(2).unwrap_or(&"")}))
        }
        "clipboard" => ("clipboard", serde_json::json!({"action": "get"})),
        "process" => ("process", serde_json::json!({"action": "list"})),
        _ => (cmd, serde_json::json!({"raw": rest})),
    };

    if !DRIVER_CONNECTED.load(Ordering::Relaxed) {
        let _ = send_telegram(&cfg.tg_token, chat_id,
            "\u{26a0}\u{fe0f} PC Driver \u{672a}\u{9023}\u{7dda}\u{ff01}\u{8acb}\u{5148}\u{5728}\u{96fb}\u{8166}\u{57f7}\u{884c} ethan_driver.py\n\
             (\u{547d}\u{4ee4}\u{5df2}\u{9001}\u{51fa}\u{ff0c}\u{82e5} Driver \u{5df2}\u{555f}\u{52d5}\u{8acb}\u{7b49}\u{5f85}\u{56de}\u{61c9})");
    }
    let _ = send_telegram(&cfg.tg_token, chat_id, &format!("Sending to PC: {} ...", cmd));
    usb_send_pc_cmd(driver_cmd, &args_json, chat_id);
}

// ===== Remind Command =====

fn handle_remind_command(cfg: &Config, state: &mut AppState, nvs: &mut EspNvs<NvsDefault>, chat_id: i64, args: &str) {
    let parts: Vec<&str> = args.splitn(2, ' ').collect();

    if parts[0] == "del" || parts[0] == "delete" {
        if let Some(id_str) = parts.get(1) {
            if let Ok(id) = id_str.trim().parse::<u32>() {
                if state.remove_reminder(id) {
                    save_reminders_to_nvs(nvs, state);
                    let _ = send_telegram(&cfg.tg_token, chat_id,
                        &format!("Deleted reminder #{}", id));
                } else {
                    let _ = send_telegram(&cfg.tg_token, chat_id,
                        &format!("Reminder #{} not found", id));
                }
                return;
            }
        }
        let _ = send_telegram(&cfg.tg_token, chat_id, "Usage: /remind del <ID>");
        return;
    }

    if parts[0] == "list" {
        let _ = send_telegram(&cfg.tg_token, chat_id, &state.list_reminders());
        return;
    }

    if parts.len() < 2 {
        let _ = send_telegram(&cfg.tg_token, chat_id,
            "Usage:\n/remind 5m message\n/remind 8:00 message\n/remind del 3");
        return;
    }

    let pattern = parts[0];
    let message = parts[1];
    let now = now_epoch();

    // Interval: 5m, 30m, 1h
    if pattern.ends_with('m') {
        if let Ok(mins) = pattern[..pattern.len()-1].parse::<u64>() {
            if mins > 0 && mins <= 1440 {
                let id = state.add_reminder(
                    ReminderType::Interval, mins * 60, 0, 0, message, now);
                save_reminders_to_nvs(nvs, state);
                let _ = send_telegram(&cfg.tg_token, chat_id,
                    &format!("Reminder #{}: every {}min \"{}\"", id, mins, message));
                return;
            }
        }
    }
    if pattern.ends_with('h') {
        if let Ok(hrs) = pattern[..pattern.len()-1].parse::<u64>() {
            if hrs > 0 && hrs <= 24 {
                let id = state.add_reminder(
                    ReminderType::Interval, hrs * 3600, 0, 0, message, now);
                save_reminders_to_nvs(nvs, state);
                let _ = send_telegram(&cfg.tg_token, chat_id,
                    &format!("Reminder #{}: every {}hr \"{}\"", id, hrs, message));
                return;
            }
        }
    }

    // Daily: HH:MM
    if pattern.contains(':') {
        let hm: Vec<&str> = pattern.split(':').collect();
        if hm.len() == 2 {
            if let (Ok(h), Ok(m)) = (hm[0].parse::<u32>(), hm[1].parse::<u32>()) {
                if h < 24 && m < 60 {
                    let id = state.add_reminder(
                        ReminderType::Daily, 0, h, m, message, now);
                    save_reminders_to_nvs(nvs, state);
                    let _ = send_telegram(&cfg.tg_token, chat_id,
                        &format!("Reminder #{}: daily {:02}:{:02} \"{}\"",
                            id, h, m, message));
                    return;
                }
            }
        }
    }

    let _ = send_telegram(&cfg.tg_token, chat_id,
        "Format error.\nUse: /remind 5m msg | /remind 8:00 msg");
}

// ===== Reminder Parser (from AI response) =====

fn parse_and_apply_reminder(reply: &str, state: &mut AppState) -> (String, bool) {
    let now = now_epoch();
    let mut clean = reply.to_string();
    let mut applied = false;

    while let Some(start) = clean.find("[REMIND:") {
        if let Some(end) = clean[start..].find(']') {
            let tag = &clean[start+8..start+end];
            let parts: Vec<&str> = tag.splitn(3, ':').collect();
            if parts.len() >= 2 {
                match parts[0] {
                    "interval" => {
                        if let Ok(mins) = parts[1].parse::<u64>() {
                            let msg = if parts.len() > 2 { parts[2] } else { "reminder" };
                            state.add_reminder(
                                ReminderType::Interval, mins * 60, 0, 0, msg, now);
                            applied = true;
                        }
                    }
                    "daily" => {
                        let time_str = parts[1];
                        let hm: Vec<&str> = time_str.split(':').collect();
                        // Handle "daily:HH:MM:msg" where parts = ["daily", "HH", "MM:msg"]
                        // or the splitn(3) may give ["daily", "08", "00:message"]
                        if hm.len() == 2 {
                            if let (Ok(h), Ok(m)) = (hm[0].parse::<u32>(), hm[1].parse::<u32>()) {
                                let msg = if parts.len() > 2 { parts[2] } else { "reminder" };
                                state.add_reminder(
                                    ReminderType::Daily, 0, h, m, msg, now);
                                applied = true;
                            }
                        } else if let Ok(h) = time_str.parse::<u32>() {
                            // parts = ["daily", "08", "00:msg"]
                            if parts.len() > 2 {
                                let rest = parts[2];
                                let sub: Vec<&str> = rest.splitn(2, ':').collect();
                                if let Ok(m) = sub[0].parse::<u32>() {
                                    let msg = if sub.len() > 1 { sub[1] } else { "reminder" };
                                    state.add_reminder(
                                        ReminderType::Daily, 0, h, m, msg, now);
                                    applied = true;
                                }
                            }
                        }
                    }
                    "once" => {
                        let time_str = parts[1];
                        if let Ok(h) = time_str.parse::<u32>() {
                            if parts.len() > 2 {
                                let rest = parts[2];
                                let sub: Vec<&str> = rest.splitn(2, ':').collect();
                                if let Ok(m) = sub[0].parse::<u32>() {
                                    let msg = if sub.len() > 1 { sub[1] } else { "reminder" };
                                    state.add_reminder(
                                        ReminderType::Once, 0, h, m, msg, now);
                                    applied = true;
                                }
                            }
                        }
                    }
                    "del" => {
                        if let Ok(id) = parts[1].parse::<u32>() {
                            state.remove_reminder(id);
                            applied = true;
                        }
                    }
                    "list" => {
                        // 自動任務特殊回覆（對比 [AUTOTASK:list] 編輯）
let remind_list = state.list_reminders();
                        clean = format!("{}", remind_list);
                    }
                    _ => {}
                }
            }
            clean = format!("{}{}", &clean[..start], clean[start+end+1..].trim_start());
        } else {
            break;
        }
    }

    (clean.trim().to_string(), applied)
}

/// 統一處理 AI 回覆中的所有命令標籤（記憶 + 自動任務），並儲存到 NVS。
/// 回傳清理後的純文字回覆。
fn parse_all_ai_commands(reply: &str, state: &mut AppState, nvs: &mut EspNvs<NvsDefault>) -> String {
    let now = now_epoch();
    let (mut clean, reminder_changed) = parse_and_apply_reminder(reply, state);

    // Parse [AUTOTASK:...] tags
    let mut autotask_changed = false;
    loop {
        let start = match clean.find("[AUTOTASK:") {
            Some(s) => s,
            None => break,
        };
        let end = match clean[start..].find(']') {
            Some(e) => e,
            None => break,
        };
        let tag = clean[start + 10..start + end].to_string();
        clean = format!("{}{}", &clean[..start], clean[start + end + 1..].trim_start());

        let parts: Vec<&str> = tag.splitn(5, ':').collect();
        if parts.is_empty() { continue; }
        match parts[0] {
            "camera" => {
                if parts.len() >= 3 {
                    if let Ok(mins_f) = parts[1].parse::<f64>() {
                        // 允許小數分鐘（0.5 = 30 秒），下限 30 秒。demo 要 30 秒一輪；
                        // 再短就是每分鐘燒兩次以上 Gemini，額度與費用都撐不住。
                        let secs: u64 = ((mins_f * 60.0).round() as u64).max(30);
                        let mins = mins_f;
                        if mins > 0.0 {
                            let prompt = parts[2].to_string();
                            let speak = parts.get(3).map_or(false, |&s| s.eq_ignore_ascii_case("speak"));
                            let id = state.next_autotask_id;
                            state.next_autotask_id += 1;
                            state.auto_tasks.push(AutoTask {
                                id,
                                interval_secs: secs,
                                next_trigger: now + secs,
                                action: "camera".to_string(),
                                prompt, speak, active: true,
                                retries_left: AUTOTASK_MAX_RETRIES,
                consecutive_failures: 0,
                            });
                            autotask_changed = true;
                            info!("AutoTask #{} added: camera every {}s", id, secs);
                        }
                    }
                }
            }
            // 條件式監看。系統提示詞一直有教 AI 用 [AUTOTASK:watch:...],但這裡少了對應
            // 分支 —— 標籤落進 _ => {} 被靜默丟掉,使用者用講的建監看任務等於沒建,
            // 而且回覆裡標籤已經被清掉,看起來像建好了。執行端(action="watch")本來就實作完整。
            "watch" => {
                if parts.len() >= 3 {
                    if let Ok(mins_f) = parts[1].parse::<f64>() {
                        // 允許小數分鐘（0.5 = 30 秒），下限 30 秒。demo 要 30 秒一輪；
                        // 再短就是每分鐘燒兩次以上 Gemini，額度與費用都撐不住。
                        let secs: u64 = ((mins_f * 60.0).round() as u64).max(30);
                        let mins = mins_f;
                        if mins > 0.0 && !parts[2].trim().is_empty() {
                            let speak = parts.get(3).map_or(false, |&s| s.eq_ignore_ascii_case("speak"));
                            let id = state.next_autotask_id;
                            state.next_autotask_id += 1;
                            state.auto_tasks.push(AutoTask {
                                id,
                                interval_secs: secs,
                                next_trigger: now + secs,
                                action: "watch".to_string(),
                                prompt: parts[2].to_string(),
                                speak,
                                active: true,
                                retries_left: AUTOTASK_MAX_RETRIES,
                consecutive_failures: 0,
                            });
                            autotask_changed = true;
                            info!("AutoTask #{} added: watch every {}s", id, secs);
                        }
                    }
                }
            }
            "del" => {
                if let Some(id_str) = parts.get(1) {
                    if let Ok(id) = id_str.parse::<u32>() {
                        let before = state.auto_tasks.len();
                        state.auto_tasks.retain(|t| t.id != id);
                        if state.auto_tasks.len() < before { autotask_changed = true; }
                    }
                }
            }
            "list" => {
                // 自動任務特殊回覆，AI 不需要再發給用戶
                let task_list = list_auto_tasks(state);
                clean = format!("{}\n{}\n{}", &clean[..start].trim(), task_list, &clean[start..].trim());
            }
            _ => {}
        }
    }

    if reminder_changed {
        save_reminders_to_nvs(nvs, state);
    }
    if autotask_changed {
        save_auto_tasks_to_nvs(nvs, state);
    }

    clean.trim().to_string()
}

fn token_footer(state: &AppState) -> String {
    let total = state.tokens_in + state.tokens_out;
    let last_in = state.tokens_in.saturating_sub(state.last_tokens_in);
    let last_out = state.tokens_out.saturating_sub(state.last_tokens_out);
    let last = last_in + last_out;
    if state.requests > 0 {
        format!("\n\n[{} | {} tok (in:{}/out:{}) | total: {} | #{}]",
            state.current_model, last, last_in, last_out, total, state.requests)
    } else {
        String::new()
    }
}

// ===== Memory System =====

fn load_memories_from_nvs(nvs: &EspNvs<NvsDefault>, state: &mut AppState) {
    let count: usize = nvs_get(nvs, "mem_cnt")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    for i in 0..count.min(MAX_MEMORIES) {
        let nkey = format!("m{:02}", i);
        if let Some(val) = nvs_get(nvs, &nkey) {
            if let Some((k, v)) = val.split_once('|') {
                state.memories.push((k.to_string(), v.to_string()));
            }
        }
    }
}

fn save_memories_to_nvs(nvs: &mut EspNvs<NvsDefault>, state: &AppState) {
    let _ = nvs.set_str("mem_cnt", &state.memories.len().to_string());
    for (i, (k, v)) in state.memories.iter().enumerate() {
        let nkey = format!("m{:02}", i);
        let combined = format!("{}|{}", k, v);
        let _ = nvs.set_str(&nkey, &combined);
    }
    for i in state.memories.len()..MAX_MEMORIES {
        let nkey = format!("m{:02}", i);
        let _ = nvs.remove(&nkey);
    }
}

// ===== Reminder NVS Persistence =====

fn save_reminders_to_nvs(nvs: &mut EspNvs<NvsDefault>, state: &AppState) {
    let active: Vec<&Reminder> = state.reminders.iter().filter(|r| r.active).collect();
    let save_count = active.len().min(MAX_REMINDERS);
    let _ = nvs.set_str("r_cnt", &save_count.to_string());
    let _ = nvs.set_str("r_nid", &state.next_reminder_id.to_string());
    for (i, r) in active.iter().enumerate().take(save_count) {
        let nkey = format!("r{:02}", i);
        let rtype_int: u8 = match r.rtype {
            ReminderType::Interval => 0,
            ReminderType::Daily => 1,
            ReminderType::Once => 2,
        };
        let val = format!("{}|{}|{}|{}|{}|{}", r.id, rtype_int, r.interval_secs, r.hour, r.minute, r.message);
        let _ = nvs.set_str(&nkey, &val);
    }
    for i in save_count..MAX_REMINDERS {
        let nkey = format!("r{:02}", i);
        let _ = nvs.remove(&nkey);
    }
    info!("Saved {} reminders to NVS", save_count);
}

fn load_reminders_from_nvs(nvs: &EspNvs<NvsDefault>, state: &mut AppState, now: u64) {
    let count: usize = nvs_get(nvs, "r_cnt")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let next_id: u32 = nvs_get(nvs, "r_nid")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    state.next_reminder_id = next_id.max(1);
    for i in 0..count.min(MAX_REMINDERS) {
        let nkey = format!("r{:02}", i);
        if let Some(val) = nvs_get(nvs, &nkey) {
            let parts: Vec<&str> = val.splitn(6, '|').collect();
            if parts.len() < 6 { continue; }
            let id: u32 = parts[0].parse().unwrap_or(0);
            let rtype_int: u8 = parts[1].parse().unwrap_or(0);
            let interval_secs: u64 = parts[2].parse().unwrap_or(60);
            let hour: u32 = parts[3].parse().unwrap_or(0);
            let minute: u32 = parts[4].parse().unwrap_or(0);
            let message = parts[5].to_string();
            let rtype = match rtype_int {
                1 => ReminderType::Daily,
                2 => ReminderType::Once,
                _ => ReminderType::Interval,
            };
            let next_trigger = match rtype {
                ReminderType::Daily => next_daily_trigger(hour, minute, now),
                ReminderType::Interval => now + interval_secs,
                ReminderType::Once => {
                    let t = next_daily_trigger(hour, minute, now);
                    if t <= now { continue; } // already passed, skip
                    t
                }
            };
            state.reminders.push(Reminder {
                id, rtype, interval_secs, next_trigger, hour, minute, message, active: true,
            });
        }
    }
    info!("Loaded {} reminders from NVS", state.reminders.len());
}

// ===== AutoTask NVS Persistence =====

fn save_auto_tasks_to_nvs(nvs: &mut EspNvs<NvsDefault>, state: &AppState) {
    let active: Vec<&AutoTask> = state.auto_tasks.iter().filter(|t| t.active).collect();
    let save_count = active.len().min(MAX_AUTO_TASKS);
    let _ = nvs.set_str("at_cnt", &save_count.to_string());
    let _ = nvs.set_str("at_nid", &state.next_autotask_id.to_string());
    for (i, t) in active.iter().enumerate().take(save_count) {
        let nkey = format!("at{:02}", i);
        let speak_int = if t.speak { 1u8 } else { 0u8 };
        // Format: id|interval_secs|speak|action|prompt (splitn 5 so prompt can contain |)
        let val = format!("{}|{}|{}|{}|{}", t.id, t.interval_secs, speak_int, t.action, t.prompt);
        let _ = nvs.set_str(&nkey, &val);
    }
    for i in save_count..MAX_AUTO_TASKS {
        let nkey = format!("at{:02}", i);
        let _ = nvs.remove(&nkey);
    }
    info!("Saved {} auto tasks to NVS", save_count);
}

fn load_auto_tasks_from_nvs(nvs: &EspNvs<NvsDefault>, state: &mut AppState, now: u64) {
    let count: usize = nvs_get(nvs, "at_cnt")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let next_id: u32 = nvs_get(nvs, "at_nid")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    state.next_autotask_id = next_id.max(1);
    for i in 0..count.min(MAX_AUTO_TASKS) {
        let nkey = format!("at{:02}", i);
        if let Some(val) = nvs_get(nvs, &nkey) {
            let parts: Vec<&str> = val.splitn(5, '|').collect();
            if parts.len() < 5 { continue; }
            let id: u32 = parts[0].parse().unwrap_or(0);
            let interval_secs: u64 = parts[1].parse().unwrap_or(300);
            let speak = parts[2] == "1";
            let action = parts[3].to_string();
            let prompt = parts[4].to_string();
            state.auto_tasks.push(AutoTask {
                id, interval_secs,
                next_trigger: now + interval_secs, // restart interval from each boot
                action, prompt, speak, active: true,
                retries_left: AUTOTASK_MAX_RETRIES,
                consecutive_failures: 0,
            });
        }
    }
    info!("Loaded {} auto tasks from NVS", state.auto_tasks.len());
}

// ===== 條件式監看(watch)任務 =====

/// 監看任務的判讀結果。
struct WatchVerdict {
    hit: bool,
    detail: String,
}

/// 把 `[ROI:x1,y1,x2,y2]條件` 拆成(框選區域, 條件文字)。
/// 座標是畫面百分比(0-100),由控制台上用手指/滑鼠拉框產生。
/// 沒有框選就回 `(None, 原字串)`。
fn strip_roi_prefix(raw: &str) -> (Option<(u8, u8, u8, u8)>, String) {
    let rest = match raw.strip_prefix("[ROI:") {
        Some(r) => r,
        None => return (None, raw.to_string()),
    };
    let (coords, tail) = match rest.split_once(']') {
        Some(pair) => pair,
        None => return (None, raw.to_string()),
    };
    // 先用 u32 收再夾到 0-100:直接 parse::<u8>() 的話 "999" 會解析失敗被濾掉,
    // 剩三個數字就整組 ROI 被丟棄,而且 "[ROI:...]" 原文會被當成條件送進提示詞。
    let nums: Vec<u8> = coords
        .split(',')
        .filter_map(|n| n.trim().parse::<u32>().ok())
        .map(|n| n.min(100) as u8)
        .collect();
    if nums.len() == 4 {
        (Some((nums[0], nums[1], nums[2], nums[3])), tail.trim().to_string())
    } else {
        (None, raw.to_string())
    }
}

/// 把使用者隨口講的一句話,包成「一定會回 JSON」的視覺判讀提示詞。
///
/// 使用者不會寫 prompt —— 他們會打「幫我看教室有沒有人睡著」。這個函式就是
/// skill 導正 prompt 的實作:固定輸出 schema、固定判斷語氣、把框選區域翻成
/// 空間描述,讓韌體端能可靠地 parse,而不是拿到一段散文。
fn build_watch_prompt(stored: &str) -> String {
    let (roi, cond) = strip_roi_prefix(stored);
    let region = match roi {
        Some((x1, y1, x2, y2)) => format!(
            "只判讀畫面中「左上角({}%,{}%) 到 右下角({}%,{}%)」這塊矩形區域，其餘部分一律忽略。\n",
            x1, y1, x2, y2
        ),
        None => String::new(),
    };
    format!(
        "你是監看判讀器。{region}要判斷的條件：{cond}\n\
         \n\
         只輸出一行 JSON，不要 markdown 代碼框、不要任何說明文字：\n\
         {{\"hit\": true 或 false, \"detail\": \"25 字以內的繁體中文說明\", \"value\": \"讀到的數字或關鍵字，沒有就空字串\"}}\n\
         \n\
         規則：\n\
         - 條件成立才 hit=true；不確定、看不清楚、畫面被遮住 → hit=false。\n\
         - hit=false 時 detail 也要填(給日誌看)，但使用者不會收到通知。\n\
         - 如果條件是要讀數字(產量、溫度、計數)，讀到就 hit=true 並把數字放進 value。",
        region = region,
        cond = cond
    )
}

/// 從 Gemini 回覆裡撈出那行 JSON。模型偶爾會包 ```json 或多寫一句話,
/// 所以取第一個 `{` 到最後一個 `}` 之間再 parse,不強求整段都是純 JSON。
fn parse_watch_verdict(raw: &str) -> Option<WatchVerdict> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end <= start {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&raw[start..=end]).ok()?;
    let hit = v.get("hit").and_then(|h| h.as_bool()).unwrap_or(false);
    let mut detail = v
        .get("detail")
        .and_then(|d| d.as_str())
        .unwrap_or("")
        .to_string();
    if let Some(value) = v.get("value").and_then(|x| x.as_str()) {
        if !value.is_empty() {
            detail = if detail.is_empty() {
                value.to_string()
            } else {
                format!("{}（{}）", detail, value)
            };
        }
    }
    if detail.is_empty() {
        detail = "條件成立".to_string();
    }
    Some(WatchVerdict { hit, detail })
}

/// 直接建立一個定期任務(供設定工具的任務建構器用,不經 AI)。回傳新任務 ID。
fn add_auto_task(state: &mut AppState, mins: u64, action: &str, prompt: &str, speak: bool) -> u32 {
    let now = now_epoch();
    let id = state.next_autotask_id;
    state.next_autotask_id += 1;
    state.auto_tasks.push(AutoTask {
        id,
        interval_secs: mins * 60,
        next_trigger: now + mins * 60,
        action: action.to_string(),
        prompt: prompt.to_string(),
        speak,
        active: true,
        retries_left: AUTOTASK_MAX_RETRIES,
                consecutive_failures: 0,
    });
    id
}

fn list_auto_tasks(state: &AppState) -> String {
    let active: Vec<&AutoTask> = state.auto_tasks.iter().filter(|t| t.active).collect();
    if active.is_empty() {
        return "📭 目前沒有進行中的自動任務\n\n新增方式：告訴 AI 你要的任務，例如：\n每5分鐘拍照問誰在睡覺用講的\n或：/tasks clear".to_string();
    }
    let now = now_epoch();
    let mut s = format!("📋 自動任務清單 ({} 個):\n", active.len());
    for t in &active {
        let mins = t.interval_secs / 60;
        let secs = t.interval_secs % 60;
        let interval_str = if secs == 0 { format!("每{}分", mins) } else { format!("每{}分{}秒", mins, secs) };
        let speak_str = if t.speak { "📢語音" } else { "📨Telegram" };
        let countdown = if t.next_trigger > now {
            let rem = t.next_trigger - now;
            if rem >= 60 { format!("{}分{}秒後", rem / 60, rem % 60) } else { format!("{}秒後", rem) }
        } else {
            "已觸發/等待".to_string()
        };
        let kind = match t.action.as_str() {
            "watch" => "👁️監看(有事才通知)",
            "camera" => "📷拍照",
            "gpio" => "⚡GPIO",
            "alert" => "🚨警報",
            other => other,
        };
        s.push_str(&format!("  #{} {} {} {} {}\n", t.id, kind, interval_str, speak_str, countdown));
        if !t.prompt.is_empty() {
            // watch 的 prompt 前面可能帶 [ROI:...] 框選座標,列表只給人看條件本身
            let shown = strip_roi_prefix(&t.prompt).1;
            let short = if shown.chars().count() > 20 {
                format!("{}...", shown.chars().take(20).collect::<String>())
            } else {
                shown
            };
            s.push_str(&format!("     ??{}\n", short));
        }
    }
    s.push_str("\n────────────────\n");
    s.push_str("🗑️ /tasks del [ID編號]\n");
    s.push_str("清除全部：/tasks clear");
    s
}

fn add_memory(nvs: &mut EspNvs<NvsDefault>, state: &mut AppState, key: &str, value: &str) {
    if let Some(item) = state.memories.iter_mut().find(|(k, _)| k == key) {
        item.1 = value.to_string();
    } else if state.memories.len() < MAX_MEMORIES {
        state.memories.push((key.to_string(), value.to_string()));
    } else {
        state.memories.remove(0);
        state.memories.push((key.to_string(), value.to_string()));
    }
    save_memories_to_nvs(nvs, state);
    info!("Memory saved: {} = {}", key, value);
}

fn remove_memory(nvs: &mut EspNvs<NvsDefault>, state: &mut AppState, key: &str) -> bool {
    let len = state.memories.len();
    state.memories.retain(|(k, _)| k != key);
    if state.memories.len() < len {
        save_memories_to_nvs(nvs, state);
        info!("Memory removed: {}", key);
        true
    } else {
        false
    }
}

fn format_memories(memories: &[(String, String)]) -> String {
    if memories.is_empty() {
        return "No memories stored.\n/remember <key> <value> to add".to_string();
    }
    let mut s = format!("Memories ({}/{}):\n", memories.len(), MAX_MEMORIES);
    for (k, v) in memories {
        s.push_str(&format!("  {} = {}\n", k, v));
    }
    s.push_str("\n/remember <key> <val>\n/forget <key>");
    s
}

fn parse_memory_tags(reply: &str, nvs: &mut EspNvs<NvsDefault>, state: &mut AppState) -> (String, bool) {
    let mut clean = reply.to_string();
    let mut applied = false;

    while let Some(start) = clean.find("[MEM:") {
        if let Some(end) = clean[start..].find(']') {
            let tag = &clean[start+5..start+end];
            if let Some(rest) = tag.strip_prefix("save:") {
                if let Some((key, val)) = rest.split_once(':') {
                    add_memory(nvs, state, key.trim(), val.trim());
                    applied = true;
                }
            } else if let Some(key) = tag.strip_prefix("del:") {
                remove_memory(nvs, state, key.trim());
                applied = true;
            }
            clean = format!("{}{}", &clean[..start], clean[start+end+1..].trim_start());
        } else {
            break;
        }
    }

    (clean.trim().to_string(), applied)
}

// ===== Python Code Tag Parser =====

/// Parse [PY:code] and [PYBLOCK]...[/PYBLOCK] tags
fn parse_py_tags(reply: &str, chat_id: i64) -> (String, bool) {
    // PC Safe Mode: python_exec is a dangerous command
    if !pc_cmd_allowed("python_exec", true) {
        // Strip tags but don't execute
        let mut clean = reply.to_string();
        // 結尾標籤一定要從開頭標籤「之後」找:AI 若回 [/PYBLOCK][PYBLOCK],
        // 從整串找會拿到前面那個,字串越刪越長 → 無限迴圈。
        while let Some(s) = clean.find("[PYBLOCK]") {
            if let Some(e) = clean[s + 9..].find("[/PYBLOCK]").map(|e| s + 9 + e) {
                clean = format!("{}{}", &clean[..s], clean[e+10..].trim_start());
            } else { break; }
        }
        while let Some(s) = clean.find("[PY:") {
            if let Some(e) = clean[s..].find(']') {
                clean = format!("{}{}", &clean[..s], clean[s+e+1..].trim_start());
            } else { break; }
        }
        if clean.trim() != reply.trim() {
            warn!("PC Safe Mode: blocked AI python_exec tags");
        }
        return (clean.trim().to_string(), false);
    }

    let mut clean = reply.to_string();
    let mut applied = false;

    // Handle [PYBLOCK]...[/PYBLOCK] first
    while let Some(start) = clean.find("[PYBLOCK]") {
        // 同上:從開頭標籤之後找結尾,否則錯序標籤會讓 start+9 > end 而 panic。
        if let Some(end) = clean[start + 9..].find("[/PYBLOCK]").map(|e| start + 9 + e) {
            let code = &clean[start+9..end];
            usb_send_pc_cmd("python_exec", &serde_json::json!({
                "code": code.trim(),
                "mode": "block"
            }), chat_id);
            applied = true;
            clean = format!("{}{}", &clean[..start], clean[end+10..].trim_start());
        } else {
            break;
        }
    }

    // Handle [PY:code] inline
    while let Some(start) = clean.find("[PY:") {
        if let Some(end) = clean[start..].find(']') {
            let code = &clean[start+4..start+end];
            usb_send_pc_cmd("python_exec", &serde_json::json!({
                "code": code,
                "mode": "inline"
            }), chat_id);
            applied = true;
            clean = format!("{}{}", &clean[..start], clean[start+end+1..].trim_start());
        } else {
            break;
        }
    }

    (clean.trim().to_string(), applied)
}

// ===== Image Generation Tag Parser =====

/// Parse [IMG:prompt] tags
fn parse_img_tags(reply: &str, chat_id: i64) -> (String, bool) {
    if !pc_cmd_allowed("image_gen", true) {
        let mut clean = reply.to_string();
        while let Some(s) = clean.find("[IMG:") {
            if let Some(e) = clean[s..].find(']') {
                clean = format!("{}{}", &clean[..s], clean[s+e+1..].trim_start());
            } else { break; }
        }
        if clean.trim() != reply.trim() {
            warn!("PC Safe Mode: blocked AI image_gen tags");
        }
        return (clean.trim().to_string(), false);
    }

    let mut clean = reply.to_string();
    let mut applied = false;

    while let Some(start) = clean.find("[IMG:") {
        if let Some(end) = clean[start..].find(']') {
            let prompt = &clean[start+5..start+end];
            usb_send_pc_cmd("image_gen", &serde_json::json!({
                "prompt": prompt,
            }), chat_id);
            applied = true;
            clean = format!("{}{}", &clean[..start], clean[start+end+1..].trim_start());
        } else {
            break;
        }
    }

    (clean.trim().to_string(), applied)
}

// ===== Excel Pipeline Tag Parser =====

/// Parse [EXCEL:path>>>instruction] tags
fn parse_excel_tags(reply: &str, chat_id: i64) -> (String, bool) {
    if !pc_cmd_allowed("excel", true) {
        let mut clean = reply.to_string();
        while let Some(s) = clean.find("[EXCEL:") {
            if let Some(e) = clean[s..].find(']') {
                clean = format!("{}{}", &clean[..s], clean[s+e+1..].trim_start());
            } else { break; }
        }
        if clean.trim() != reply.trim() {
            warn!("PC Safe Mode: blocked AI excel tags");
        }
        return (clean.trim().to_string(), false);
    }

    let mut clean = reply.to_string();
    let mut applied = false;

    while let Some(start) = clean.find("[EXCEL:") {
        if let Some(end) = clean[start..].find(']') {
            let tag = &clean[start+7..start+end];
            let parts: Vec<&str> = tag.splitn(2, ">>>").collect();
            let path = parts.first().unwrap_or(&"");
            let instruction = parts.get(1).unwrap_or(&"");
            usb_send_pc_cmd("excel_pipeline", &serde_json::json!({
                "path": path,
                "instruction": instruction,
            }), chat_id);
            applied = true;
            clean = format!("{}{}", &clean[..start], clean[start+end+1..].trim_start());
        } else {
            break;
        }
    }

    (clean.trim().to_string(), applied)
}

// ===== Bot Info =====

fn fetch_bot_username(token: &str) -> Result<String> {
    let url = format!("https://api.telegram.org/bot{}/getMe", token);
    let resp = http_get(&url)?;
    // parse "username":"xxx" from JSON
    if let Some(pos) = resp.find("\"username\"") {
        let rest = &resp[pos + 10..];
        if let Some(start) = rest.find('"') {
            let rest2 = &rest[start + 1..];
            if let Some(end) = rest2.find('"') {
                return Ok(rest2[..end].to_string());
            }
        }
    }
    bail!("username not found in getMe response");
}

// ===== OTA System =====

fn perform_ota(firmware: &[u8]) -> Result<()> {
    if firmware.len() < 1024 {
        bail!("Firmware too small ({} bytes)", firmware.len());
    }
    if firmware.len() > 4 * 1024 * 1024 {
        bail!("Firmware too large ({} bytes)", firmware.len());
    }

    info!("OTA starting: {} bytes", firmware.len());

    unsafe {
        let partition = esp_idf_svc::sys::esp_ota_get_next_update_partition(std::ptr::null());
        if partition.is_null() {
            bail!("No OTA partition found");
        }

        let mut handle: u32 = 0;
        let err = esp_idf_svc::sys::esp_ota_begin(
            partition,
            firmware.len(),
            &mut handle as *mut u32 as *mut _,
        );
        if err != 0 {
            bail!("esp_ota_begin failed: err={}", err);
        }

        info!("OTA writing to partition...");
        for (i, chunk) in firmware.chunks(4096).enumerate() {
            let err = esp_idf_svc::sys::esp_ota_write(
                handle,
                chunk.as_ptr() as *const core::ffi::c_void,
                chunk.len(),
            );
            if err != 0 {
                bail!("OTA write chunk {} failed: err={}", i, err);
            }
            // 寫 flash 是同步阻塞的,3MB 韌體要跑 700 多個 chunk、耗時遠超過
            // 60 秒的 task WDT 預算。這個迴圈原本一次都沒餵狗 —— OTA 幾乎必然
            // 在寫到一半時被踢重開,而那正是最不能被打斷的時候。
            feed_watchdog();
            if (i + 1) % 100 == 0 {
                info!("OTA: {}KB / {}KB", (i + 1) * 4, firmware.len() / 1024);
            }
        }

        let err = esp_idf_svc::sys::esp_ota_end(handle);
        if err != 0 {
            bail!("OTA validation failed: err={}", err);
        }

        let err = esp_idf_svc::sys::esp_ota_set_boot_partition(partition);
        if err != 0 {
            bail!("Set boot partition failed: err={}", err);
        }

        info!("OTA complete! Ready to reboot.");
    }

    Ok(())
}

fn handle_ota_url(cfg: &Config, chat_id: i64, url: &str) {
    if !url.starts_with("https://") {
        let _ = send_telegram(&cfg.tg_token, chat_id, "⚠️ 安全限制：OTA 只接受 HTTPS URL。\nUsage: /ota https://...");
        return;
    }

    let _ = send_telegram(&cfg.tg_token, chat_id,
        &format!("\u{2b07}\u{fe0f} Downloading firmware...\n{}", url));

    match http_get_binary(url) {
        Ok(data) => {
            let _ = send_telegram(&cfg.tg_token, chat_id,
                &format!("Downloaded {} bytes. Starting OTA...", data.len()));
            match perform_ota(&data) {
                Ok(()) => {
                    let _ = send_telegram(&cfg.tg_token, chat_id,
                        &format!("\u{2705} OTA success! {} bytes\nRebooting in 3s...", data.len()));
                    std::thread::sleep(Duration::from_secs(3));
                    unsafe { esp_idf_svc::sys::esp_restart(); }
                }
                Err(e) => {
                    let _ = send_telegram(&cfg.tg_token, chat_id,
                        &format!("\u{274c} OTA failed: {}", e));
                }
            }
        }
        Err(e) => {
            let _ = send_telegram(&cfg.tg_token, chat_id,
                &format!("Download failed: {}", e));
        }
    }
}

fn handle_ota_document(cfg: &Config, chat_id: i64, file_id: &str, file_name: &str) {
    let _ = send_telegram(&cfg.tg_token, chat_id,
        &format!("\u{1f4e6} Firmware: {}\nDownloading...", file_name));

    let file_url = format!("{}{}/getFile?file_id={}", TELEGRAM_API, cfg.tg_token, file_id);
    let file_resp = match http_get(&file_url) {
        Ok(r) => r,
        Err(e) => {
            let _ = send_telegram(&cfg.tg_token, chat_id, &format!("getFile failed: {}", e));
            return;
        }
    };

    let file_info: TgFileResponse = match serde_json::from_str(&file_resp) {
        Ok(f) => f,
        Err(e) => {
            let _ = send_telegram(&cfg.tg_token, chat_id, &format!("Parse error: {}", e));
            return;
        }
    };

    let file_path = match file_info.result.and_then(|r| r.file_path) {
        Some(p) => p,
        None => {
            let _ = send_telegram(&cfg.tg_token, chat_id, "File path not found");
            return;
        }
    };

    let download_url = format!("{}{}/{}", TELEGRAM_FILE_API, cfg.tg_token, file_path);
    let data = match http_get_binary(&download_url) {
        Ok(d) => d,
        Err(e) => {
            let _ = send_telegram(&cfg.tg_token, chat_id, &format!("Download failed: {}", e));
            return;
        }
    };

    let _ = send_telegram(&cfg.tg_token, chat_id,
        &format!("Downloaded {} bytes. Starting OTA...", data.len()));

    match perform_ota(&data) {
        Ok(()) => {
            let _ = send_telegram(&cfg.tg_token, chat_id,
                &format!("\u{2705} OTA success! {} bytes\nRebooting in 3s...", data.len()));
            std::thread::sleep(Duration::from_secs(3));
            unsafe { esp_idf_svc::sys::esp_restart(); }
        }
        Err(e) => {
            let _ = send_telegram(&cfg.tg_token, chat_id,
                &format!("\u{274c} OTA failed: {}", e));
        }
    }
}

// ===== Gemini API =====

/// Gate check before any Gemini API call.
/// Returns Ok(()) if allowed, Err with user-facing message if blocked.
fn gemini_gate() -> Result<()> {
    if PAUSED.load(Ordering::Relaxed) {
        bail!("⏸️ 系統已暫停，所有 AI 呼叫已停止。\n傳送 /resume 恢復。");
    }
    let now = now_epoch() as u32; // lower 32 bits, fine for 60s windows
    let win_start = GEMINI_WINDOW_START.load(Ordering::Relaxed);
    if now.wrapping_sub(win_start) >= GEMINI_RATE_WINDOW_SECS as u32 {
        // New window
        GEMINI_WINDOW_START.store(now, Ordering::Relaxed);
        GEMINI_CALL_COUNT.store(1, Ordering::Relaxed);
        Ok(())
    } else {
        let count = GEMINI_CALL_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if count > MAX_GEMINI_CALLS_PER_WINDOW {
            GEMINI_CALL_COUNT.fetch_sub(1, Ordering::Relaxed); // undo
            let remaining = (GEMINI_RATE_WINDOW_SECS as u32).saturating_sub(now.wrapping_sub(win_start));
            bail!("🚫 已達到速率限制（每 {}秒 最多 {} 次 AI 呼叫）。\n請等待 {} 秒後再試。",
                GEMINI_RATE_WINDOW_SECS, MAX_GEMINI_CALLS_PER_WINDOW, remaining);
        }
        Ok(())
    }
}

fn ask_gemini_with_context(api_key: &str, question: &str, state: &mut AppState) -> Result<String> {
    gemini_gate()?;

    // Save token counts before this request for accurate per-request tracking
    state.last_tokens_in = state.tokens_in;
    state.last_tokens_out = state.tokens_out;

    let url = format!("{}/{}:generateContent?key={}", GEMINI_API, state.current_model, api_key);

    let mut contents: Vec<GeminiContent> = Vec::new();
    for (role, text) in &state.history {
        contents.push(GeminiContent {
            role: role.clone(),
            parts: vec![GeminiPartOut::Text { text: text.clone() }],
        });
    }
    contents.push(GeminiContent {
        role: "user".into(),
        parts: vec![GeminiPartOut::Text { text: question.to_string() }],
    });

    let mut sys_text = build_system_prompt(state);
    if !state.memories.is_empty() {
        sys_text.push_str("\n\nUser's memories:\n");
        for (k, v) in &state.memories {
            sys_text.push_str(&format!("- {}: {}\n", k, v));
        }
    }
    // Append skills to system prompt
    sys_text.push_str(&format_skills_for_prompt(&state.skills));
    // Append recent diagnostics for self-awareness
    sys_text.push_str(&state.diag.format_for_gemini());

    let request = GeminiRequest {
        system_instruction: GeminiSysInstr {
            parts: vec![GeminiTextPart { text: sys_text }],
        },
        contents,
        generation_config: GenConfig {
            max_output_tokens: 4096,   // 從 2048 升至 4096，防止長回覆被截斷
temperature: 0.3,
        },
    };

    let json_body = serde_json::to_string(&request)?;
    let resp_body = http_post(&url, &json_body)?;

    // Check for API-level errors (expired key, quota, etc.)
    if let Ok(err_obj) = serde_json::from_str::<serde_json::Value>(&resp_body) {
        if let Some(err) = err_obj.get("error") {
            let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
            let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown error");
            error!("Gemini API error {}: {}", code, msg);
            state.diag.log("ERR", &format!("Gemini API {}: {}", code, str_prefix(&msg, 80)));
            bail!("[FACE:sad] \u{274c} Gemini API \u{932f}\u{8aa4} ({}): {}", code, msg);
        }
    }

    let resp: GeminiResponse = serde_json::from_str(&resp_body).map_err(|e| {
        error!("Gemini parse err: {} body_len={}", e, resp_body.len());
        e
    })?;

    if let Some(usage) = &resp.usage_metadata {
        state.tokens_in += usage.prompt_token_count.unwrap_or(0);
        state.tokens_out += usage.candidates_token_count.unwrap_or(0);
        state.requests += 1;
    }

    // Extract finish_reason from first candidate (if any) — clone to owned before move
    let finish_reason: String = resp.candidates.as_ref()
        .and_then(|c| c.first())
        .and_then(|c| c.finish_reason.clone())
        .unwrap_or_else(|| "UNKNOWN".to_string());

    let answer = resp.candidates
        .and_then(|c| c.into_iter().next())
        .and_then(|c| c.content)
        .and_then(|c| c.parts)
        .and_then(|p| p.into_iter().next())
        .and_then(|p| p.text);

    let answer = match answer {
        Some(text) if !text.trim().is_empty() => text,
        _ => {
            // Diagnose why Gemini returned no content
            let block_reason = resp.prompt_feedback
                .as_ref()
                .and_then(|pf| pf.block_reason.as_deref());
            // 只記長度不記內容:README 承諾「聊天內容不記錄到 Serial」。
            warn!("Gemini empty reply — blockReason={:?} finishReason={} q_len={}", block_reason, finish_reason, question.len());
            state.diag.log("WARN", &format!("Gemini empty: block={:?} finish={}", block_reason, finish_reason));
            match block_reason {
                Some("SAFETY") => "[FACE:sad] \u{26a0}\u{fe0f} Gemini \u{5b89}\u{5168}\u{7b56}\u{7565}\u{5c01}\u{9396}\u{4e86}\u{6b64}\u{8a0a}\u{606f}\u{3002}\u{8acb}\u{8a66}\u{8457}\u{91cd}\u{65b0}\u{63cf}\u{8ff0}\u{4f60}\u{7684}\u{554f}\u{984c}\u{3002}".to_string(),
                Some("PROHIBITED_CONTENT") => "[FACE:sad] \u{26a0}\u{fe0f} Gemini \u{4ee5}\u{300c}\u{7981}\u{6b62}\u{5167}\u{5bb9}\u{300d}\u{62d2}\u{7d55}\u{56de}\u{61c9}\u{3002}\u{8acb}\u{4fee}\u{6539}\u{554f}\u{984c}\u{5f8c}\u{518d}\u{8a66}\u{3002}".to_string(),
                Some(reason) => format!("[FACE:sad] \u{26a0}\u{fe0f} Gemini \u{62d2}\u{7d55}\u{56de}\u{61c9}\u{ff08}\u{539f}\u{56e0}: {}\u{ff09}\u{3002}\u{8acb}\u{91cd}\u{65b0}\u{63cf}\u{8ff0}\u{4f60}\u{7684}\u{554f}\u{984c}\u{3002}", reason),
                None if finish_reason == "SAFETY" => "[FACE:sad] \u{26a0}\u{fe0f} Gemini \u{56de}\u{61c9}\u{88ab}\u{5b89}\u{5168}\u{904e}\u{6ffe}\u{5668}\u{6514}\u{622a}\u{3002}\u{8acb}\u{8a66}\u{8457}\u{63db}\u{500b}\u{65b9}\u{5f0f}\u{63cf}\u{8ff0}\u{3002}".to_string(),
                None if finish_reason == "RECITATION" => "[FACE:thinking] \u{26a0}\u{fe0f} Gemini \u{56e0}\u{7248}\u{6b0a}\u{4fdd}\u{8b77}\u{62d2}\u{7d55}\u{56de}\u{61c9}\u{3002}\u{8acb}\u{7528}\u{81ea}\u{5df1}\u{7684}\u{8a71}\u{63cf}\u{8ff0}\u{554f}\u{984c}\u{3002}".to_string(),
                None => "[FACE:thinking] \u{2753} Gemini \u{8fd4}\u{56de}\u{4e86}\u{7a7a}\u{56de}\u{61c9}\u{3002}\u{8acb}\u{518d}\u{8a66}\u{4e00}\u{6b21}\u{ff0c}\u{6216}\u{8a66}\u{8457}\u{91cd}\u{65b0}\u{63cf}\u{8ff0}\u{4f60}\u{7684}\u{554f}\u{984c}\u{3002}".to_string(),
            }
        }
    };

    state.add_history("user", question);
    state.add_history("model", &answer);

    Ok(answer)
}

fn ask_gemini_with_image_context(
    api_key: &str,
    question: &str,
    image_data: &[u8],
    mime_type: &str,
    state: &mut AppState,
) -> Result<String> {
    gemini_gate()?;
    state.last_tokens_in = state.tokens_in;
    state.last_tokens_out = state.tokens_out;

    let url = format!("{}/{}:generateContent?key={}", GEMINI_API, state.current_model, api_key);

    let mut contents: Vec<GeminiContent> = Vec::new();
    for (role, text) in &state.history {
        contents.push(GeminiContent {
            role: role.clone(),
            parts: vec![GeminiPartOut::Text { text: text.clone() }],
        });
    }
    contents.push(GeminiContent {
        role: "user".into(),
        parts: vec![
            GeminiPartOut::Text {
                text: question.to_string(),
            },
            GeminiPartOut::InlineData {
                inline_data: InlineData {
                    mime_type: mime_type.to_string(),
                    data: base64_encode(image_data),
                },
            },
        ],
    });

    let mut sys_text = build_system_prompt(state);
    sys_text.push_str("\n\nThe user may attach Telegram photos. When an image is present, analyze the actual image content before answering.");
    if !state.memories.is_empty() {
        sys_text.push_str("\n\nUser's memories:\n");
        for (k, v) in &state.memories {
            sys_text.push_str(&format!("- {}: {}\n", k, v));
        }
    }
    sys_text.push_str(&format_skills_for_prompt(&state.skills));
    sys_text.push_str(&state.diag.format_for_gemini());

    let request = GeminiRequest {
        system_instruction: GeminiSysInstr {
            parts: vec![GeminiTextPart { text: sys_text }],
        },
        contents,
        generation_config: GenConfig {
            max_output_tokens: 4096,   // 從 2048 升至 4096，防止視覺回覆被中斷
            temperature: 0.5,
        },
    };

    let json_body = serde_json::to_string(&request)?;
    let resp_body = http_post(&url, &json_body)?;

    // Check for API-level errors
    if let Ok(err_obj) = serde_json::from_str::<serde_json::Value>(&resp_body) {
        if let Some(err) = err_obj.get("error") {
            let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
            let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown error");
            error!("Gemini image API error {}: {}", code, msg);
            bail!("[FACE:sad] \u{274c} Gemini API \u{932f}\u{8aa4} ({}): {}", code, msg);
        }
    }

    let resp: GeminiResponse = serde_json::from_str(&resp_body).map_err(|e| {
        error!("Gemini image parse err: {} body_len={}", e, resp_body.len());
        e
    })?;

    if let Some(usage) = &resp.usage_metadata {
        state.tokens_in += usage.prompt_token_count.unwrap_or(0);
        state.tokens_out += usage.candidates_token_count.unwrap_or(0);
        state.requests += 1;
    }

    let img_finish: String = resp.candidates.as_ref()
        .and_then(|c| c.first())
        .and_then(|c| c.finish_reason.clone())
        .unwrap_or_else(|| "UNKNOWN".to_string());

    let answer = resp.candidates
        .and_then(|c| c.into_iter().next())
        .and_then(|c| c.content)
        .and_then(|c| c.parts)
        .and_then(|p| p.into_iter().next())
        .and_then(|p| p.text);

    let answer = match answer {
        Some(text) if !text.trim().is_empty() => text,
        _ => {
            let block_reason = resp.prompt_feedback
                .as_ref()
                .and_then(|pf| pf.block_reason.as_deref());
            warn!("Gemini image empty — blockReason={:?} finishReason={}", block_reason, img_finish);
            state.diag.log("WARN", &format!("Gemini img empty: block={:?} finish={}", block_reason, img_finish));
            match block_reason {
                Some("SAFETY") | Some("PROHIBITED_CONTENT") =>
                    "[FACE:sad] \u{26a0}\u{fe0f} Gemini \u{5b89}\u{5168}\u{7b56}\u{7565}\u{5c01}\u{9396}\u{4e86}\u{6b64}\u{5716}\u{7247}/\u{8a0a}\u{606f}\u{3002}".to_string(),
                Some(reason) => format!("[FACE:sad] \u{26a0}\u{fe0f} Gemini \u{62d2}\u{7d55}\u{56de}\u{61c9}\u{5716}\u{7247}\u{ff08}{}\u{ff09}", reason),
                None => "[FACE:thinking] \u{2753} Gemini \u{5716}\u{7247}\u{5206}\u{6790}\u{8fd4}\u{56de}\u{7a7a}\u{56de}\u{61c9}\u{3002}\u{8acb}\u{518d}\u{8a66}\u{4e00}\u{6b21}\u{3002}".to_string(),
            }
        }
    };

    state.add_history("user", &format!("[photo] {}", question));
    state.add_history("model", &answer);

    Ok(answer)
}

fn transcribe_audio_with_mime(api_key: &str, audio_data: &[u8], mime_type: &str, state: &mut AppState) -> Result<String> {
    gemini_gate()?;
    let url = format!("{}/{}:generateContent?key={}", GEMINI_API, TRANSCRIBE_MODEL, api_key);

    let b64 = base64_encode(audio_data);

    let request = GeminiRequest {
        system_instruction: GeminiSysInstr {
            parts: vec![GeminiTextPart {
                text: "You are a speech transcriber. Output ONLY the transcribed text. No extra words.".into(),
            }],
        },
        contents: vec![GeminiContent {
            role: "user".into(),
            parts: vec![
                GeminiPartOut::InlineData {
                    inline_data: InlineData {
                        mime_type: mime_type.into(),
                        data: b64,
                    },
                },
                GeminiPartOut::Text {
                    text: "Transcribe this voice message.".into(),
                },
            ],
        }],
        generation_config: GenConfig {
            max_output_tokens: 512,
            temperature: 0.1,
        },
    };

    let json_body = serde_json::to_string(&request)?;
    let resp_body = http_post(&url, &json_body)?;

    // Check for API-level errors
    if let Ok(err_obj) = serde_json::from_str::<serde_json::Value>(&resp_body) {
        if let Some(err) = err_obj.get("error") {
            let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
            let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown error");
            error!("Gemini transcribe API error {}: {}", code, msg);
            state.diag.log("ERR", &format!("Gemini API {}: {}", code, str_prefix(&msg, 80)));
            bail!("\u{274c} Gemini API \u{932f}\u{8aa4} ({}): {}", code, msg);
        }
    }

    let resp: GeminiResponse = serde_json::from_str(&resp_body)?;

    if let Some(usage) = &resp.usage_metadata {
        state.tokens_in += usage.prompt_token_count.unwrap_or(0);
        state.tokens_out += usage.candidates_token_count.unwrap_or(0);
        state.requests += 1;
    }

    resp.candidates
        .and_then(|c| c.into_iter().next())
        .and_then(|c| c.content)
        .and_then(|c| c.parts)
        .and_then(|p| p.into_iter().next())
        .and_then(|p| p.text)
        .ok_or_else(|| anyhow::anyhow!("\u{274c} \u{8a9e}\u{97f3}\u{8f49}\u{5beb}\u{5931}\u{6557}\u{ff0c}Gemini \u{672a}\u{8fd4}\u{56de}\u{6587}\u{5b57}"))
}

fn transcribe_audio(api_key: &str, audio_data: &[u8], state: &mut AppState) -> Result<String> {
    transcribe_audio_with_mime(api_key, audio_data, "audio/ogg", state)
}

fn telegram_get_file_path(token: &str, file_id: &str) -> Result<String> {
    let file_url = format!("{}{}/getFile?file_id={}", TELEGRAM_API, token, file_id);
    let file_resp = http_get(&file_url)?;
    let file_info: TgFileResponse = serde_json::from_str(&file_resp)?;
    if !file_info.ok {
        bail!("Telegram getFile returned not ok");
    }
    file_info
        .result
        .and_then(|r| r.file_path)
        .ok_or_else(|| anyhow::anyhow!("Telegram file path not found"))
}

fn download_telegram_file(token: &str, file_id: &str) -> Result<Vec<u8>> {
    let file_path = telegram_get_file_path(token, file_id)?;
    let download_url = format!("{}{}/{}", TELEGRAM_FILE_API, token, file_path);
    http_get_binary(&download_url)
}

fn infer_media_mime(file_name: Option<&str>, mime_type: Option<&str>) -> Option<String> {
    if let Some(mime) = mime_type {
        return Some(mime.to_string());
    }
    let ext = file_name
        .and_then(|name| name.rsplit('.').next())
        .map(|ext| ext.to_ascii_lowercase())?;
    match ext.as_str() {
        "jpg" | "jpeg" => Some("image/jpeg".to_string()),
        "png" => Some("image/png".to_string()),
        "webp" => Some("image/webp".to_string()),
        "ogg" | "oga" => Some("audio/ogg".to_string()),
        "mp3" => Some("audio/mpeg".to_string()),
        "wav" => Some("audio/wav".to_string()),
        "m4a" => Some("audio/mp4".to_string()),
        _ => None,
    }
}

fn url_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 2);
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*byte as char),
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

fn sanitize_speech_text(text: &str) -> String {
    let first = text.split("\n[").next().unwrap_or(text);
    let cleaned = first
        .replace('\n', " ")
        .replace('*', " ")
        .replace('`', " ")
        .replace('#', " ")
        .replace('_', " ");
    cleaned
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(TTS_MAX_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

/// 代理連不上後,冷卻到這個開機秒數為止都直接跳過(0=沒在冷卻)。
static TTS_PROXY_SKIP_UNTIL: AtomicU32 = AtomicU32::new(0);
/// 代理不通後的冷卻時間。電腦關機時每句話都去等一次 TCP 逾時是不能用的。
const TTS_PROXY_COOLDOWN_SECS: u32 = 300;

fn board_uptime_secs() -> u32 {
    (unsafe { esp_idf_svc::sys::esp_timer_get_time() } / 1_000_000) as u32
}

fn tts_proxy_in_cooldown() -> bool {
    let until = TTS_PROXY_SKIP_UNTIL.load(Ordering::Relaxed);
    until != 0 && board_uptime_secs() < until
}

/// 走使用者家裡電腦上的 TTS 代理拿好聽音色。失敗一律回 Err 交給呼叫端決定退路。
fn tts_download_via_proxy(
    nvs: &EspNvs<NvsDefault>,
    speech: &str,
    proxy_base: &str,
) -> Result<Vec<u8>> {
    let body = serde_json::json!({
        "text": speech,
        "voice": load_tts_proxy_voice(nvs),
        "format": "json",
    });
    let json_body = serde_json::to_string(&body)?;
    info!("TTS proxy download: {} chars via {}", speech.chars().count(), proxy_base);
    let resp_body = http_post(proxy_base, &json_body)?;
    let resp: TtsProxyJsonResponse = serde_json::from_str(&resp_body)?;
    if !resp.ok {
        bail!("tts proxy error: {}", resp.error.unwrap_or_else(|| "unknown proxy error".to_string()));
    }
    let audio_b64 = resp.audio_b64.ok_or_else(|| anyhow!("tts proxy missing audio_b64"))?;
    Ok(base64::engine::general_purpose::STANDARD.decode(audio_b64)?)
}

fn tts_download_mp3(nvs: &EspNvs<NvsDefault>, text: &str) -> Result<Vec<u8>> {
    let speech = sanitize_speech_text(text);
    if speech.is_empty() {
        bail!("speech text is empty");
    }

    // 先試好聽的代理音色。代理是跑在使用者家裡電腦上的,電腦關機/休眠/換 IP
    // 都會讓它消失 —— 那時候**必須自己退回 Google 語音**,不能就這樣啞掉。
    // (原本這裡是 if/else:設了代理就只走代理,失敗直接 bail,裝置全程無聲
    //  而且使用者完全不知道為什麼。)
    let proxy_mp3 = match load_tts_proxy_url(nvs) {
        None => None,
        Some(_) if tts_proxy_in_cooldown() => {
            info!("TTS 代理冷卻中,直接用 Google 語音");
            None
        }
        Some(proxy_base) => match tts_download_via_proxy(nvs, &speech, &proxy_base) {
            Ok(data) => {
                TTS_PROXY_SKIP_UNTIL.store(0, Ordering::Relaxed); // 通了就解除冷卻
                Some(data)
            }
            Err(e) => {
                // 連不上要等 TCP 逾時(實測約 18 秒),每句話都卡這麼久沒法用。
                // 記下來冷卻一段時間直接跳過,期間全部走 Google。
                let until = board_uptime_secs().saturating_add(TTS_PROXY_COOLDOWN_SECS);
                TTS_PROXY_SKIP_UNTIL.store(until.max(1), Ordering::Relaxed);
                warn!("TTS 代理不通({:?}),改用 Google 語音,{} 秒內不再重試",
                      e, TTS_PROXY_COOLDOWN_SECS);
                None
            }
        },
    };

    let mp3 = if let Some(data) = proxy_mp3 {
        data
    } else {
        info!("TTS fallback download: {} chars", speech.chars().count());
        // Try multiple Google TTS client IDs to reduce rate-limit failures
        let clients = ["tw-ob", "gtx", "t"];
        let mut mp3_result: Result<Vec<u8>> = Err(anyhow::anyhow!("TTS: no clients tried"));
        for client in &clients {
            let url = format!(
                // ttsspeed 原本寫 0.88 —— 那是比正常慢 12%,退回 Google 時聽起來
                // 又機械又拖。Google 這個端點的上限就是 1(給更大值會被夾回 1),
                // 所以退回音最快只能到正常速;要 1.2 倍以上得靠 TTS 代理。
                "https://translate.google.com/translate_tts?ie=UTF-8&client={}&tl=zh-TW&ttsspeed=1&q={}",
                client,
                url_encode(&speech)
            );
            let try_result = http_get_binary_with_headers(
                &url,
                &[
                    ("User-Agent", "Mozilla/5.0 (Linux; Android 10) AppleWebKit/537.36 NANA/4.7"),
                    ("Accept", "audio/mpeg,audio/*;q=0.9,*/*;q=0.5"),
                    ("Referer", "https://translate.google.com/"),
                    ("Accept-Language", "zh-TW,zh;q=0.9,en;q=0.7"),
                ],
            );
            match &try_result {
                Ok(data) if data.len() > 100 => {
                    info!("TTS client='{}' OK: {} bytes", client, data.len());
                    mp3_result = try_result;
                    break;
                }
                Ok(data) => warn!("TTS client='{}' got {} bytes (too small), trying next", client, data.len()),
                Err(e) => warn!("TTS client='{}' failed: {:?}, trying next", client, e),
            }
        }
        mp3_result?
    };

    info!("TTS download OK: {} bytes", mp3.len());
    Ok(mp3)
}

fn decode_mp3_to_pcm(mp3_data: &[u8]) -> Result<(Vec<i16>, u32, usize)> {
    let mut decoder = Mp3Decoder::new();
    // 先估容量再解碼。Vec 從 0 成長要反覆倍增,長回覆最後會停在 2MB 的單一配置,
    // 而擴充的那一瞬間新舊兩塊同時存在(再吃 1MB)—— 那正是記憶體最緊的時候。
    // 24kHz 單聲道 MP3 大約 1 byte 壓縮資料對到 5 個 i16 sample,抓 6 倍留餘裕;
    // 估錯也只是回到原本的成長行為,不會壞。
    let est_samples = (mp3_data.len() * 6).min(4 * 1024 * 1024);
    let mut pcm: Vec<i16> = Vec::with_capacity(est_samples);
    let mut sample_rate = 0u32;
    let mut channels = 1usize;
    let mut offset = 0usize;
    let mut frame_pcm = vec![0f32; MP3_MAX_SAMPLES_PER_FRAME];
    let mut frame_count = 0usize;

    while offset < mp3_data.len() {
        let (consumed, info) = decoder.decode(&mp3_data[offset..], &mut frame_pcm[..]);
        if consumed == 0 {
            break;
        }
        offset += consumed;

        if let Some(info) = info {
            frame_count += 1;
            if sample_rate == 0 {
                sample_rate = info.sample_rate.max(8_000);
                channels = info.channels.num() as usize;
            }
            for sample in frame_pcm[..info.samples_produced].iter() {
                let scaled = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                pcm.push(scaled);
            }
            // 一段 30 秒的回覆有一千多個 frame,整段解碼是純 CPU 工作、期間主任務
            // 不會回頭餵狗。跟播放迴圈同樣的問題,每 64 個 frame 餵一次就夠。
            if frame_count % 64 == 0 {
                feed_watchdog();
            }
        }
    }

    if pcm.is_empty() || sample_rate == 0 {
        bail!("decoded mp3 produced no pcm samples");
    }

    info!(
        "TTS decode OK: {} frames, {} samples, {} Hz, {} ch",
        frame_count,
        pcm.len(),
        sample_rate,
        channels
    );

    Ok((pcm, sample_rate, channels))
}

fn normalize_tts_pcm_for_board(
    pcm: &[i16],
    input_rate: u32,
    input_channels: usize,
    target_rate: u32,
) -> Result<(Vec<i16>, u32, usize)> {
    if pcm.is_empty() {
        bail!("tts pcm is empty");
    }

    let channels = input_channels.max(1);
    let target_fast = target_rate.max(8_000);
    const GAIN: f32 = 0.42;

    // 快路徑:單聲道 + 取樣率已相符,是實際最常走的情況(代理與 Google 都給
    // 24kHz 單聲道)。原本會先 `pcm.to_vec()` 複製一份**內容完全相同**的資料,
    // 再走一次 map+collect,等於為一段 30 秒的回覆白白配置 1.44MB。
    // 這裡直接從輸入切片一次產出結果,少一次整段配置。
    if channels == 1 && input_rate == target_fast {
        let shaped: Vec<i16> = pcm
            .iter()
            .map(|s| ((*s as f32 * GAIN).clamp(i16::MIN as f32, i16::MAX as f32)) as i16)
            .collect();
        info!(
            "TTS normalized for board: {} -> {} Hz, 1 ch -> mono, {} samples (fast path)",
            input_rate, target_fast, shaped.len()
        );
        return Ok((shaped, target_fast, 1));
    }

    let mono = if channels == 1 {
        pcm.to_vec()
    } else {
        let mut mixed = Vec::with_capacity(pcm.len() / channels.max(1));
        for frame in pcm.chunks(channels) {
            let sum: i32 = frame.iter().map(|sample| *sample as i32).sum();
            mixed.push((sum / frame.len() as i32) as i16);
        }
        mixed
    };

    let target = target_rate.max(8_000);
    let resampled = if input_rate == target {
        mono
    } else {
        let out_len = ((mono.len() as u64 * target as u64) / input_rate.max(1) as u64).max(1) as usize;
        let mut out = Vec::with_capacity(out_len);
        for idx in 0..out_len {
            let src_pos = idx as f32 * input_rate as f32 / target as f32;
            let base = src_pos.floor() as usize;
            let frac = src_pos - base as f32;
            let current = mono.get(base).copied().unwrap_or_default() as f32;
            let next = mono.get(base + 1).copied().unwrap_or(current as i16) as f32;
            let interpolated = current + ((next - current) * frac);
            out.push(interpolated as i16);
        }
        out
    };

    let gain = 0.42f32;
    let shaped = resampled
        .into_iter()
        .map(|sample| ((sample as f32 * gain).clamp(i16::MIN as f32, i16::MAX as f32)) as i16)
        .collect::<Vec<_>>();

    info!(
        "TTS normalized for board: {} -> {} Hz, {} ch -> mono, {} samples",
        input_rate,
        target,
        channels,
        shaped.len()
    );

    Ok((shaped, target, 1))
}

fn pcm16_to_wav_bytes(samples: &[i16], sample_rate: u32, channels: u16) -> Vec<u8> {
    let bits_per_sample = 16u16;
    let byte_rate = sample_rate * channels as u32 * (bits_per_sample as u32 / 8);
    let block_align = channels * (bits_per_sample / 8);
    let data_len = (samples.len() * 2) as u32;
    let riff_len = 36 + data_len;

    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&riff_len.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&bits_per_sample.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

fn speak_onboard_text(nvs: &EspNvs<NvsDefault>, text: &str) -> Result<String> {
    let speech = sanitize_speech_text(text);
    if speech.is_empty() {
        bail!("speech text is empty");
    }

    let audio_pins = load_audio_pins(nvs);
    let key = tts_cache_key(&speech);

    // Try TTS cache first
    let cached = TTS_CACHE.lock().ok().and_then(|c| {
        c.iter().find(|(k, _)| *k == key).map(|(_, mp3)| mp3.clone())
    });

    let mp3 = if let Some(mp3) = cached {
        info!("TTS cache hit: {} chars", speech.chars().count());
        mp3
    } else {
        info!("TTS download: {} chars", speech.chars().count());
        let mp3 = tts_download_mp3(nvs, &speech)?;
        if let Ok(mut cache) = TTS_CACHE.lock() {
            if cache.len() >= TTS_CACHE_MAX {
                cache.remove(0);
            }
            cache.push((key, mp3.clone()));
        }
        mp3
    };

    let (pcm, sample_rate, channels) = decode_mp3_to_pcm(&mp3)?;
    let (pcm, sample_rate, channels) = normalize_tts_pcm_for_board(
        &pcm,
        sample_rate,
        channels,
        audio_pins.sample_rate,
    )?;

    info!("TTS playback start: {} pcm samples", pcm.len());
    play_pcm16(&audio_pins, &pcm, sample_rate, channels)?;
    info!("TTS playback done");
    Ok(speech)
}

fn capture_and_transcribe_mic(
    cfg: &Config,
    nvs: &EspNvs<NvsDefault>,
    state: &mut AppState,
    duration_ms: u32,
) -> Result<String> {
    let audio_pins = load_audio_pins(nvs);
    let pcm = capture_mic_pcm(
        &audio_pins,
        duration_ms,
        audio_pins.sample_rate as usize * duration_ms as usize / 1000 + 2048,
    )?;
    let wav = pcm16_to_wav_bytes(&pcm, audio_pins.sample_rate, 1);
    transcribe_audio_with_mime(&cfg.gemini_key, &wav, "audio/wav", state)
}

fn normalize_for_match(input: &str) -> String {
    input
        .chars()
        .flat_map(|ch| ch.to_lowercase())
        .filter(|ch| !ch.is_whitespace() && !ch.is_ascii_punctuation())
        .collect()
}

fn should_speak_onboard(mode: &str) -> bool {
    matches!(mode, "normal" | "brief")
}

/// brief 模式至少要唸這麼多字才算「有講到重點」。
/// 中文口語很愛用「你好呀!」「好呀!」開頭,只切第一句會變成唸三個字就停,
/// 聽起來跟壞掉沒兩樣 —— 所以要一直吃下一句直到夠長為止。
const BRIEF_MIN_CHARS: usize = 22;
/// brief 模式的上限,超過就不再往下吃。
const BRIEF_MAX_CHARS: usize = 60;

fn speech_text_for_mode(mode: &str, text: &str) -> String {
    let sanitized = sanitize_speech_text(text);
    if mode != "brief" {
        return sanitized;
    }
    let is_end = |c: char| matches!(c, '\u{3002}' | '\u{ff01}' | '\u{ff1f}' | '.' | '!' | '?');
    let mut out = String::new();
    let mut count = 0usize;
    for ch in sanitized.chars() {
        out.push(ch);
        count += 1;
        // 句尾標點:字數夠了才收工,不夠就繼續吃下一句
        if is_end(ch) && count >= BRIEF_MIN_CHARS {
            return out;
        }
        if count >= BRIEF_MAX_CHARS {
            return out;
        }
    }
    out
}

fn detect_wake_phrase(
    cfg: &Config,
    nvs: &EspNvs<NvsDefault>,
    state: &mut AppState,
    duration_ms: u32,
) -> Result<Option<String>> {
    let audio_pins = load_audio_pins(nvs);
    let max_samples = audio_pins.sample_rate as usize * duration_ms as usize / 1000 + 2048;
    let pcm = capture_mic_pcm(&audio_pins, duration_ms, max_samples)?;

    // Try ESP-SR WakeNet first (local, free, fast)
    let has_sr = WAKENET.lock().ok().map_or(false, |g| g.is_some());
    if has_sr {
        let pcm16k = if audio_pins.sample_rate != 16000 {
            resample_linear(&pcm, audio_pins.sample_rate, 16000)
        } else {
            pcm.clone()
        };
        if let Some(word) = detect_wake_sr(&pcm16k) {
            return Ok(Some(word));
        }
        // WakeNet only recognises its built-in phrase (Hi,ESP).
        // Fall through to Gemini STT with the SAME audio for custom phrases.
    }

    // RMS silence check: skip Gemini API call if audio is too quiet (no speech)
    let energy: u64 = pcm.iter().map(|&s| (s as i64 * s as i64) as u64).sum();
    let rms = if !pcm.is_empty() {
        ((energy / pcm.len() as u64) as f64).sqrt() as u32
    } else {
        0
    };
    // 1500 not 800: 電視/風扇背景噪音 RMS 常落在 800-1400,原本每輪喚醒檢查
    // (約 18 秒一次)都會白燒一次 Gemini STT 且全部失敗,還把主迴圈拖慢。
    if rms < 1500 {
        return Ok(None); // 靜音或背景噪音,不呼叫 Gemini
    }

    // 雲端轉寫退避:連續失敗 3 次(通常=噪音而非人聲)休息 5 分鐘,
    // 保護 API 額度也還給主迴圈時間。本地 WakeNet 偵測不受影響。
    let now_u = now_epoch().max(0) as u32;
    if now_u < WAKE_STT_BACKOFF_UNTIL.load(Ordering::Relaxed) {
        return Ok(None);
    }

    // Cloud fallback: reuse captured audio for Gemini STT transcription
    let wav = pcm16_to_wav_bytes(&pcm, audio_pins.sample_rate, 1);
    let transcript = match transcribe_audio_with_mime(&cfg.gemini_key, &wav, "audio/wav", state) {
        Ok(t) => t,
        Err(e) => {
            let fails = WAKE_STT_FAILS.fetch_add(1, Ordering::Relaxed) + 1;
            if fails >= 3 {
                WAKE_STT_BACKOFF_UNTIL.store(now_u + 300, Ordering::Relaxed);
                WAKE_STT_FAILS.store(0, Ordering::Relaxed);
                info!("喚醒雲端轉寫連續失敗,休息 5 分鐘(本地喚醒仍運作)");
            }
            return Err(e);
        }
    };
    if transcript.trim().is_empty() {
        let fails = WAKE_STT_FAILS.fetch_add(1, Ordering::Relaxed) + 1;
        if fails >= 3 {
            WAKE_STT_BACKOFF_UNTIL.store(now_u + 300, Ordering::Relaxed);
            WAKE_STT_FAILS.store(0, Ordering::Relaxed);
            info!("喚醒雲端轉寫連續空回應,休息 5 分鐘(本地喚醒仍運作)");
        }
        return Ok(None);
    }
    WAKE_STT_FAILS.store(0, Ordering::Relaxed);
    let heard = normalize_for_match(&transcript);
    let target = normalize_for_match(&state.wake_phrase);
    if !target.is_empty() && heard.contains(&target) {
        Ok(Some(transcript))
    } else {
        Ok(None)
    }
}

/// Simple linear interpolation resampling
fn resample_linear(pcm: &[i16], from_rate: u32, to_rate: u32) -> Vec<i16> {
    if from_rate == to_rate || pcm.is_empty() { return pcm.to_vec(); }
    let ratio = from_rate as f64 / to_rate as f64;
    let out_len = ((pcm.len() as f64) / ratio) as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f64 * ratio;
        let idx = src as usize;
        let frac = src - idx as f64;
        let s0 = pcm[idx] as f64;
        let s1 = if idx + 1 < pcm.len() { pcm[idx + 1] as f64 } else { s0 };
        out.push((s0 + frac * (s1 - s0)) as i16);
    }
    out
}

fn describe_image_with_gemini(api_key: &str, state: &mut AppState, jpeg_data: &[u8], prompt: &str) -> Result<String> {
    gemini_gate()?;
    let url = format!("{}/{}:generateContent?key={}", GEMINI_API, state.current_model, api_key);
    let b64 = base64_encode(jpeg_data);

    let request = GeminiRequest {
        system_instruction: GeminiSysInstr {
            parts: vec![GeminiTextPart {
                // 原本寫「請描述畫面中的人物、物件、文字，並提供執行建議」——
                // 那等於不管使用者問什麼都要求做完整場景描述加建議,造成兩個實際症狀:
                // 回太長(講滿 max_output_tokens 就從中間斷掉)、以及答非所問
                // (問「冰箱裡還有牛奶嗎」卻去描述整個畫面)。改成以使用者的問題為準。
                text: "你是 NANA 的視覺模組,看照片回答問題,用繁體中文。
                       規則:
                       - 直接回答使用者問的那件事,不要順便描述整個畫面。
                       - 除非使用者要求詳細說明,否則控制在 60 字以內,講完就停。
                       - 看不清楚或畫面裡沒有答案,就直說看不到,不要猜。
                       - 問某個項目的數值時,畫面上必須有那個項目的標籤才算數。
                         標籤對不上就回答「畫面上沒有這個項目」,絕對不要把旁邊
                         別的數字拿來充數 —— 例如問濕度、畫面上只有溫度,
                         那就是沒有濕度。貼錯標籤的讀數比沒有讀數更危險。
                       - 不要用條列、不要加開場白與結尾客套話。".into(),
            }],
        },
        contents: vec![GeminiContent {
            role: "user".into(),
            parts: vec![
                GeminiPartOut::Text {
                    text: prompt.to_string(),
                },
                GeminiPartOut::InlineData {
                    inline_data: InlineData {
                        mime_type: "image/jpeg".into(),
                        data: b64,
                    },
                },
            ],
        }],
        generation_config: GenConfig {
            max_output_tokens: 768,
            // 動態:預設 0.4,任務重試時會逐步調高(同參數重跑通常結果一樣),
            // 使用者用 /ai temp 指定的話就固定用他的值。
            temperature: current_vision_temp(),
        },
    };

    let json_body = serde_json::to_string(&request)?;
    let resp_body = http_post(&url, &json_body)?;
    let resp: GeminiResponse = serde_json::from_str(&resp_body)?;

    if let Some(usage) = &resp.usage_metadata {
        state.tokens_in += usage.prompt_token_count.unwrap_or(0);
        state.tokens_out += usage.candidates_token_count.unwrap_or(0);
        state.requests += 1;
    }

    let candidate = resp
        .candidates
        .and_then(|c| c.into_iter().next())
        .ok_or_else(|| anyhow::anyhow!("No vision response"))?;

    // 講到一半被 token 上限砍掉時,finishReason 會是 MAX_TOKENS。原本沒看這個欄位,
    // 使用者就收到一句從中間斷掉的話,還以為是裝置壞了。明講出來至少知道發生什麼事。
    let truncated = candidate
        .finish_reason
        .as_deref()
        .map(|r| r.eq_ignore_ascii_case("MAX_TOKENS"))
        .unwrap_or(false);

    let text = candidate
        .content
        .and_then(|c| c.parts)
        .and_then(|p| p.into_iter().next())
        .and_then(|p| p.text)
        .ok_or_else(|| anyhow::anyhow!("No vision response"))?;

    if truncated {
        warn!("Vision reply hit MAX_TOKENS, {} chars", text.chars().count());
        return Ok(format!("{}…
(回覆過長被截斷,換個更具體的問法會更準)", text.trim_end()));
    }
    Ok(text)
}

// ===== HTTP =====

/// HTTP 逾時的硬上限。
///
/// 必須明顯低於 task WDT(sdkconfig.defaults 的 CONFIG_ESP_TASK_WDT_TIMEOUT_S=60,
/// 那是 Kconfig 允許的最大值)。餵狗只在進入網路呼叫時做一次,呼叫過程中不會餵 ——
/// 所以任何「跑滿逾時」的請求只要 >= 60 秒,就會直接把裝置踢重開:
///
///     E task_wdt: Task watchdog got triggered ... - main (CPU 0)
///     E task_wdt: Aborting.  -> Rebooting
///
/// 壓測實測到的就是這個:代理呼叫卡住,185427ms 進入、245437ms 被踢,剛好 60 秒。
/// 原本有呼叫點傳 90 甚至 120 秒,那是「跑滿逾時必定重開」。在這裡集中夾住,
/// 呼叫點寫多少都不會再超標。
const HTTP_MAX_TIMEOUT_SECS: u64 = 40;

fn new_http_config(timeout_secs: u64) -> HttpConfig {
    new_http_config_ex(timeout_secs, false)
}

/// `follow_post_redirect` = POST 也要跟隨 302。
///
/// 預設政策是 FollowGetHead —— 只有 GET/HEAD 會跟隨轉址,POST 不會。
/// 這會擋掉一個很實用的用法:Google Apps Script 的 Web App
/// (把讀數直接寫進 Google 試算表)收到 POST 一定回 302 轉到
/// script.googleusercontent.com,不跟隨就等於寫不進去。
///
/// 只在使用者自己設定的上報端點打開。注意跟隨 POST 轉址時 Authorization
/// 標頭會一起帶到轉址目標,所以只用在「使用者自己填的網址」這條路徑上,
/// 不套用到 Gemini/Telegram 那些固定端點。
fn new_http_config_ex(timeout_secs: u64, follow_post_redirect: bool) -> HttpConfig {
    let timeout_secs = timeout_secs.min(HTTP_MAX_TIMEOUT_SECS);
    HttpConfig {
        use_global_ca_store: true,
        crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
        timeout: Some(Duration::from_secs(timeout_secs)),
        buffer_size: Some(16384),      // 加大：Gemini JSON 回覆可達 20-30KB
        buffer_size_tx: Some(8192),    // 加大：system prompt + history 輸出
        follow_redirects_policy: if follow_post_redirect {
            esp_idf_svc::http::client::FollowRedirectsPolicy::FollowAll
        } else {
            esp_idf_svc::http::client::FollowRedirectsPolicy::FollowGetHead
        },
        ..Default::default()
    }
}

fn http_get(url: &str) -> Result<String> {
    // 進入前餵一次:多個網路呼叫串聯時,每次進入都重置看門狗預算。
    // 注意預算是 60 秒不是 90(Kconfig 上限就是 60,寫 90 會被打回 5 秒預設值),
    // 所以 HTTP 逾時由 HTTP_MAX_TIMEOUT_SECS 夾在 40 秒,留得出餘裕。
    feed_watchdog();
    let mut client = EspHttpConnection::new(&new_http_config(30))?;
    client.initiate_request(esp_idf_svc::http::Method::Get, url, &[])?;
    client.initiate_response()?;
    LAST_ONLINE.store(uptime_secs(), Ordering::Relaxed);

    let mut body = Vec::with_capacity(4096);
    let mut buf = [0u8; 2048];
    loop {
        match client.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&buf[..n]),
            Err(e) => { warn!("read: {:?}", e); break; }
        }
    }
    Ok(String::from_utf8_lossy(&body).to_string())
}

fn http_get_binary(url: &str) -> Result<Vec<u8>> {
    http_get_binary_with_headers(url, &[])
}

fn http_get_binary_with_headers(url: &str, headers: &[(&str, &str)]) -> Result<Vec<u8>> {
    info!("HTTP GET binary: create client");
    let mut client = EspHttpConnection::new(&new_http_config(120))?;
    // Mask sensitive tokens in URL for logging
    let safe_url = if url.contains("/bot") {
        url.split("/bot").next().unwrap_or("").to_string() + "/bot***"
    } else {
        url.to_string()
    };
    // 每個階段都要餵狗。HTTP config 的 timeout 是**每次 socket 操作**的逾時,
    // 不是整趟請求的總時長 —— initiate_request 與 initiate_response 各自可以吃滿
    // 逾時,body 讀取又是另一段。全程不餵狗的話,一個大檔下載必定超過 60 秒的
    // task WDT 預算而重開,OTA(數 MB)幾乎是必然。
    feed_watchdog();
    info!("HTTP GET binary: initiate request {}", safe_url);
    client.initiate_request(esp_idf_svc::http::Method::Get, url, headers)?;
    feed_watchdog();
    info!("HTTP GET binary: await response");
    client.initiate_response()?;
    feed_watchdog();

    let mut body = Vec::with_capacity(64 * 1024);
    let mut buf = [0u8; 4096];
    info!("HTTP GET binary: reading body");
    loop {
        match client.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                body.extend_from_slice(&buf[..n]);
                feed_watchdog(); // 大檔要讀上千次,每次都餵才撐得過
                if body.len() > 4 * 1024 * 1024 {
                    bail!("File too large (>4MB)");
                }
            }
            Err(e) => { warn!("read: {:?}", e); break; }
        }
    }
    info!("HTTP GET binary: complete {} bytes", body.len());
    Ok(body)
}

fn http_post(url: &str, json_body: &str) -> Result<String> {
    feed_watchdog(); // 多個網路呼叫串聯時,每次進入都重置 90 秒預算
    let body_bytes = json_body.as_bytes();
    let content_len = body_bytes.len().to_string();

    let mut client = EspHttpConnection::new(&new_http_config(60))?;
    client.initiate_request(
        esp_idf_svc::http::Method::Post,
        url,
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &content_len),
        ],
    )?;
    client.write_all(body_bytes)?;
    client.initiate_response()?;
    LAST_ONLINE.store(uptime_secs(), Ordering::Relaxed);

    let mut body = Vec::with_capacity(8192);
    let mut buf = [0u8; 2048];
    loop {
        match client.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&buf[..n]),
            Err(e) => { warn!("read: {:?}", e); break; }
        }
    }
    Ok(String::from_utf8_lossy(&body).to_string())
}

fn http_post_bytes(url: &str, headers: &[(&str, &str)], body_bytes: &[u8]) -> Result<String> {
    http_post_bytes_ex(url, headers, body_bytes, false)
}

fn http_post_bytes_ex(url: &str, headers: &[(&str, &str)], body_bytes: &[u8],
                      follow_post_redirect: bool) -> Result<String> {
    let content_len = body_bytes.len().to_string();
    let mut request_headers: Vec<(&str, &str)> = headers.to_vec();
    request_headers.push(("Content-Length", &content_len));

    let mut client = EspHttpConnection::new(
        &new_http_config_ex(90, follow_post_redirect))?;
    client.initiate_request(
        esp_idf_svc::http::Method::Post,
        url,
        &request_headers,
    )?;
    client.write_all(body_bytes)?;
    client.initiate_response()?;
    LAST_ONLINE.store(uptime_secs(), Ordering::Relaxed);
    // 只在使用者自訂的上報端點驗狀態碼（follow_post_redirect 只有那條路徑會開）。
    // Telegram 與 Gemini 的錯誤是靠讀回應 body 判斷的（409 衝突、error 物件），
    // 那兩條不能在這裡 bail。沒有這段的話 401/404/500 全算「上報成功」，
    // 還會把失敗計數歸零，/report test 永遠綠勾——客戶的資料庫其實一筆都沒收到。
    if follow_post_redirect {
        let st = client.status();
        if !(200..300).contains(&st) {
            let mut pb = [0u8; 200];
            let n = client.read(&mut pb).unwrap_or(0);
            let peek = String::from_utf8_lossy(&pb[..n]).trim().to_string();
            bail!("HTTP {} {}", st, peek);
        }
    }

    let mut body = Vec::with_capacity(8192);
    let mut buf = [0u8; 2048];
    loop {
        match client.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&buf[..n]),
            Err(e) => {
                warn!("read: {:?}", e);
                break;
            }
        }
    }
    Ok(String::from_utf8_lossy(&body).to_string())
}


// ===== Telegram =====

fn poll_telegram(token: &str, offset: i64, timeout_secs: u32) -> Result<Vec<TgUpdate>> {
    let url = format!(
        "{}{}/getUpdates?offset={}&timeout={}&allowed_updates=[\"message\"]",
        TELEGRAM_API, token, offset, timeout_secs
    );
    let body = http_get(&url)?;
    // 409 Conflict = 另一台裝置正用「相同 Telegram token」輪詢同一個 bot。
    // Telegram 的 getUpdates 只允許一個輪詢者,兩台會互搶 → 兩台都收不到訊息。
    // 這就是「同時開兩台 ESP 就出 bug」的根因。給每台不同的 bot token 即可解決。
    if body.contains("\"error_code\":409") || body.contains("terminated by other getUpdates") {
        TG_CONFLICT.store(true, Ordering::Relaxed);
        bail!("TG_CONFLICT: 另一台裝置用相同 Telegram token 在輪詢(請給每台不同的 bot token)");
    }
    TG_CONFLICT.store(false, Ordering::Relaxed);
    let resp: TgResponse = serde_json::from_str(&body)?;
    if !resp.ok {
        bail!("Telegram API error");
    }
    Ok(resp.result.unwrap_or_default())
}

fn prepare_telegram_polling(token: &str, nvs: &EspNvs<NvsDefault>) -> Result<i64> {
    let delete_url = format!("{}{}/deleteWebhook?drop_pending_updates=false", TELEGRAM_API, token);
    let _ = http_get(&delete_url)?;
    Ok(load_last_telegram_update_id(nvs))
}

fn split_telegram_text(text: &str, max_len: usize) -> Vec<String> {
    if text.chars().count() <= max_len {
        return vec![text.to_string()];
    }

    let chars: Vec<char> = text.chars().collect();
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let mut end = (start + max_len).min(chars.len());
        if end < chars.len() {
            for idx in (start..end).rev() {
                if chars[idx].is_whitespace() || chars[idx] == '\n' {
                    end = idx + 1;
                    break;
                }
            }
            if end <= start {
                end = (start + max_len).min(chars.len());
            }
        }
        let chunk: String = chars[start..end].iter().collect();
        chunks.push(chunk.trim().to_string());
        start = end;
    }
    chunks.retain(|chunk| !chunk.is_empty());
    chunks
}

fn send_telegram(token: &str, chat_id: i64, text: &str) -> Result<()> {
    let url = format!("{}{}/sendMessage", TELEGRAM_API, token);
    for chunk in split_telegram_text(text, 3800) {
        let payload = serde_json::json!({
            "chat_id": chat_id,
            "text": chunk
        });
        let resp = http_post(&url, &payload.to_string())?;
        ensure_telegram_api_ok(&resp)?;
        info!("Sent ({}B)", resp.len());
        std::thread::sleep(Duration::from_millis(120));
    }
    Ok(())
}

fn send_telegram_camera_jpeg(token: &str, chat_id: i64, jpeg: &[u8], caption: &str) -> Result<()> {
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=3 {
        match send_telegram_photo_jpeg(token, chat_id, jpeg, caption) {
            Ok(()) => return Ok(()),
            Err(photo_err) => {
                warn!("sendPhoto attempt {} failed: {}", attempt, photo_err);
                match send_telegram_document_jpeg(token, chat_id, jpeg, caption) {
                    Ok(()) => return Ok(()),
                    Err(doc_err) => {
                        warn!("sendDocument attempt {} failed: {}", attempt, doc_err);
                        last_err = Some(anyhow!("photo err: {}; document err: {}", photo_err, doc_err));
                        std::thread::sleep(Duration::from_millis(400 * attempt as u64));
                    }
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("telegram jpeg upload failed after retries")))
}

fn send_telegram_photo_jpeg(token: &str, chat_id: i64, jpeg: &[u8], caption: &str) -> Result<()> {
    let boundary = "----ethan-photo-boundary";
    let mut body = Vec::with_capacity(jpeg.len() + 1024);
    let chat_id_text = chat_id.to_string();
    let caption_text = str_prefix(caption, 900);

    body.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"chat_id\"\r\n\r\n");
    body.extend_from_slice(chat_id_text.as_bytes());
    body.extend_from_slice(b"\r\n");

    body.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"caption\"\r\n\r\n");
    body.extend_from_slice(caption_text.as_bytes());
    body.extend_from_slice(b"\r\n");

    body.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"photo\"; filename=\"ethan.jpg\"\r\n");
    body.extend_from_slice(b"Content-Type: image/jpeg\r\n\r\n");
    body.extend_from_slice(jpeg);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{}--\r\n", boundary).as_bytes());

    let url = format!("{}{}/sendPhoto", TELEGRAM_API, token);
    let content_type = format!("multipart/form-data; boundary={}", boundary);
    let headers = [("Content-Type", content_type.as_str())];
    let resp = http_post_bytes(&url, &headers, &body)?;
    ensure_telegram_api_ok(&resp)?;
    info!("Photo sent ({}B)", resp.len());
    Ok(())
}

fn send_telegram_document_jpeg(token: &str, chat_id: i64, jpeg: &[u8], caption: &str) -> Result<()> {
    let boundary = "----ethan-doc-boundary";
    let mut body = Vec::with_capacity(jpeg.len() + 1024);
    let chat_id_text = chat_id.to_string();
    let caption_text = str_prefix(caption, 900);

    body.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"chat_id\"\r\n\r\n");
    body.extend_from_slice(chat_id_text.as_bytes());
    body.extend_from_slice(b"\r\n");

    body.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"caption\"\r\n\r\n");
    body.extend_from_slice(caption_text.as_bytes());
    body.extend_from_slice(b"\r\n");

    body.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"document\"; filename=\"ethan-camera.jpg\"\r\n");
    body.extend_from_slice(b"Content-Type: image/jpeg\r\n\r\n");
    body.extend_from_slice(jpeg);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{}--\r\n", boundary).as_bytes());

    let url = format!("{}{}/sendDocument", TELEGRAM_API, token);
    let content_type = format!("multipart/form-data; boundary={}", boundary);
    let headers = [("Content-Type", content_type.as_str())];
    let resp = http_post_bytes(&url, &headers, &body)?;
    ensure_telegram_api_ok(&resp)?;
    info!("Document sent ({}B)", resp.len());
    Ok(())
}

// ===== Base64 =====

fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

