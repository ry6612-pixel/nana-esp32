use anyhow::{anyhow, bail, Result};
use display_interface_spi::SPIInterface;
use embedded_graphics::image::{Image, ImageRaw};
use embedded_graphics::mono_font::ascii::FONT_6X10;
use embedded_graphics::mono_font::MonoTextStyleBuilder;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Circle, PrimitiveStyleBuilder, Rectangle, RoundedRectangle};
use embedded_graphics::text::{Alignment, Text};
use embedded_hal::spi::MODE_0;
use esp_idf_hal::delay::Ets;
use esp_idf_hal::gpio::{AnyIOPin, AnyOutputPin, PinDriver};
use esp_idf_hal::i2c::{I2cConfig, I2cDriver};
use esp_idf_hal::i2s::config::{DataBitWidth, StdConfig};
use esp_idf_hal::i2s::{I2sDriver, I2sRx, I2sTx};
use esp_idf_hal::peripherals::Peripherals;
use esp_idf_hal::spi::{self, SpiDeviceDriver, SpiDriverConfig};
use esp_idf_hal::units::FromValueType;
use esp_idf_sys::{gpio_mode_t_GPIO_MODE_OUTPUT, gpio_reset_pin, gpio_set_direction, gpio_set_level};
use esp_idf_svc::nvs::{EspNvs, NvsDefault};
use log::{info, warn};
use mipidsi::models::ST7789;
use mipidsi::options::{ColorInversion, ColorOrder, Orientation};
use mipidsi::Builder;
use serde::{Deserialize, Serialize};
use std::f32::consts::TAU;
use std::thread;
use std::time::{Duration, Instant};

pub const LCD_NVS_KEY: &str = "lcd_pins";
pub const AUDIO_NVS_KEY: &str = "audio_pins";
const BOARD_IO_SDA_PIN: i32 = 1;
const BOARD_IO_SCL_PIN: i32 = 2;
const BOARD_IO_EXPANDER_ADDRS: [u8; 4] = [0x18, 0x19, 0x1A, 0x1B];
const ES8311_ADDRS: [u8; 2] = [0x18, 0x19];
const ES8311_RESET_REG00: u8 = 0x00;
const ES8311_CLK_MANAGER_REG01: u8 = 0x01;
const ES8311_CLK_MANAGER_REG02: u8 = 0x02;
const ES8311_CLK_MANAGER_REG03: u8 = 0x03;
const ES8311_CLK_MANAGER_REG04: u8 = 0x04;
const ES8311_CLK_MANAGER_REG05: u8 = 0x05;
const ES8311_CLK_MANAGER_REG06: u8 = 0x06;
const ES8311_CLK_MANAGER_REG07: u8 = 0x07;
const ES8311_CLK_MANAGER_REG08: u8 = 0x08;
const ES8311_SDPIN_REG09: u8 = 0x09;
const ES8311_SDPOUT_REG0A: u8 = 0x0A;
const ES8311_SYSTEM_REG0B: u8 = 0x0B;
const ES8311_SYSTEM_REG0C: u8 = 0x0C;
const ES8311_SYSTEM_REG0D: u8 = 0x0D;
const ES8311_SYSTEM_REG0E: u8 = 0x0E;
const ES8311_SYSTEM_REG10: u8 = 0x10;
const ES8311_SYSTEM_REG11: u8 = 0x11;
const ES8311_SYSTEM_REG12: u8 = 0x12;
const ES8311_SYSTEM_REG13: u8 = 0x13;
const ES8311_SYSTEM_REG14: u8 = 0x14;
const ES8311_ADC_REG15: u8 = 0x15;
const ES8311_ADC_REG16: u8 = 0x16;
const ES8311_ADC_REG17: u8 = 0x17;
const ES8311_ADC_REG1B: u8 = 0x1B;
const ES8311_ADC_REG1C: u8 = 0x1C;
const ES8311_DAC_REG31: u8 = 0x31;
const ES8311_DAC_REG32: u8 = 0x32;
const ES8311_DAC_REG37: u8 = 0x37;
const ES8311_GPIO_REG44: u8 = 0x44;
const ES8311_GP_REG45: u8 = 0x45;
const ES8311_CHD1_REGFD: u8 = 0xFD;
const ES8311_CHD2_REGFE: u8 = 0xFE;
const ES8311_CHVER_REGFF: u8 = 0xFF;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LcdPins {
    pub rst: i32,
    pub dc: i32,
    pub bl: i32,
    pub sclk: i32,
    pub sda: i32,
    pub cs: i32,
    pub width: u16,
    pub height: u16,
}

impl Default for LcdPins {
    fn default() -> Self {
        Self {
            rst: 21,
            dc: 47,
            bl: 38,
            sclk: 19,
            sda: 20,
            cs: 45,
            width: 240,
            height: 320,
        }
    }
}

impl LcdPins {
    pub fn status_text(&self) -> String {
        format!(
            "ST7789 pins\nrst={} dc={} bl={} sclk={} sda={} cs={}\nsize={}x{}",
            self.rst, self.dc, self.bl, self.sclk, self.sda, self.cs, self.width, self.height
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioPins {
    pub bclk: i32,
    pub ws: i32,
    pub dout: i32,
    pub din: i32,
    #[serde(default = "default_mic_sck_pin")]
    pub mic_sck: i32,
    #[serde(default = "default_mic_ws_pin")]
    pub mic_ws: i32,
    #[serde(default = "default_mic_din_pin")]
    pub mic_din: i32,
    pub mclk: i32,
    pub sample_rate: u32,
}

fn default_mic_sck_pin() -> i32 { 2 }
fn default_mic_ws_pin() -> i32 { 1 }
fn default_mic_din_pin() -> i32 { 42 }

impl Default for AudioPins {
    fn default() -> Self {
        Self {
            bclk: 40,
            ws: 41,
            dout: 39,
            din: 42,
            mic_sck: default_mic_sck_pin(),
            mic_ws: default_mic_ws_pin(),
            mic_din: default_mic_din_pin(),
            mclk: -1,
            sample_rate: 24_000,
        }
    }
}

impl AudioPins {
    pub fn status_text(&self) -> String {
        format!(
            "Speaker pins\nbclk={} ws={} dout={} mclk={}\nMic pins\nws={} sck={} din={}\nrate={}Hz",
            self.bclk, self.ws, self.dout, self.mclk, self.mic_ws, self.mic_sck, self.mic_din, self.sample_rate
        )
    }
}

#[derive(Clone, Debug)]
pub struct MicSnapshot {
    pub rms: u32,
    pub peak: i16,
    pub samples: usize,
}

pub fn load_lcd_pins(nvs: &EspNvs<NvsDefault>) -> LcdPins {
    super::nvs_get(nvs, LCD_NVS_KEY)
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save_lcd_pins(nvs: &mut EspNvs<NvsDefault>, pins: &LcdPins) -> Result<()> {
    nvs.set_str(LCD_NVS_KEY, &serde_json::to_string(pins)?)?;
    Ok(())
}

pub fn parse_lcd_pin_args(input: &str, pins: &mut LcdPins) -> Result<()> {
    for token in input.split_whitespace() {
        let (key, value) = token
            .split_once('=')
            .ok_or_else(|| anyhow!("bad lcd token: {}", token))?;
        match key {
            "rst" => pins.rst = value.parse()?,
            "dc" => pins.dc = value.parse()?,
            "bl" => pins.bl = value.parse()?,
            "sclk" => pins.sclk = value.parse()?,
            "sda" => pins.sda = value.parse()?,
            "cs" => pins.cs = value.parse()?,
            "width" => pins.width = value.parse()?,
            "height" => pins.height = value.parse()?,
            _ => bail!("unknown lcd field: {}", key),
        }
    }
    Ok(())
}

pub fn load_audio_pins(nvs: &EspNvs<NvsDefault>) -> AudioPins {
    super::nvs_get(nvs, AUDIO_NVS_KEY)
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save_audio_pins(nvs: &mut EspNvs<NvsDefault>, pins: &AudioPins) -> Result<()> {
    nvs.set_str(AUDIO_NVS_KEY, &serde_json::to_string(pins)?)?;
    Ok(())
}

pub fn parse_audio_pin_args(input: &str, pins: &mut AudioPins) -> Result<()> {
    for token in input.split_whitespace() {
        let (key, value) = token
            .split_once('=')
            .ok_or_else(|| anyhow!("bad audio token: {}", token))?;
        match key {
            "bclk" => pins.bclk = value.parse()?,
            "ws" => pins.ws = value.parse()?,
            "dout" => pins.dout = value.parse()?,
            "din" => {
                let parsed = value.parse()?;
                pins.din = parsed;
                pins.mic_din = parsed;
            }
            "mic_sck" | "mic_bclk" => pins.mic_sck = value.parse()?,
            "mic_ws" => pins.mic_ws = value.parse()?,
            "mic_din" => pins.mic_din = value.parse()?,
            "mclk" => pins.mclk = value.parse()?,
            "rate" | "sample_rate" => pins.sample_rate = value.parse()?,
            _ => bail!("unknown audio field: {}", key),
        }
    }
    Ok(())
}

pub fn show_boot_screen(pins: &LcdPins) -> Result<()> {
    draw_lcd_scene(pins, "NANA", "Booting hardware...")
}

pub fn show_ready_screen(pins: &LcdPins) -> Result<()> {
    draw_lcd_scene(pins, "NANA", "Telegram + Gemini ready")
}

/// 最後一次畫螢幕的開機秒數,用來判斷「現在到底閒不閒」。
static LAST_DRAW_AT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// 待機換臉的間隔。距離上次畫面更新不到這個時間就跳過,
/// 免得剛回完話沒多久就被待機畫面蓋掉。
const IDLE_FACE_INTERVAL_SECS: u32 = 300;

/// 配網模式專用畫面:整片螢幕拿來講「怎麼設定」,不放臉。
///
/// 這是使用者拿到裝置後看到的第一個畫面,而且他手上只有手機、沒有說明書,
/// 所以三件事要一眼看完:連哪個 WiFi、開哪個網址、PIN 幾號。網址與 PIN
/// 用兩倍大(32px)顯示 —— 這是要照著打進手機的東西,長輩不該瞇眼睛看。
pub fn show_provisioning_screen(pins: &LcdPins, ssid: &str, url: &str, pin: &str) -> Result<()> {
    with_lcd_display(pins, LcdScene::Provisioning { ssid, url, pin })?;
    info!("LCD 配網畫面: SSID={} URL={} PIN={}", ssid, url, pin);
    Ok(())
}

/// 加密熱點的配網畫面:網址那一行換成「熱點密碼」。
/// 手機連上熱點後 captive portal 會自己跳設定頁,網址不必照抄;熱點密碼不抄就連不上。
pub fn show_provisioning_screen_secured(pins: &LcdPins, ssid: &str, ap_pass: &str, pin: &str) -> Result<()> {
    with_lcd_display(pins, LcdScene::ProvisioningSecured { ssid, ap_pass, pin })?;
    info!("LCD 配網畫面(加密熱點): SSID={} 熱點密碼={} PIN={}", ssid, ap_pass, pin);
    Ok(())
}

/// 已連上 WiFi 之後的資訊畫面 —— 跟配網畫面對稱。
///
/// 配網時螢幕告訴使用者「連哪個 WiFi、開哪個網址、PIN 幾號」;連上之後那些資訊
/// 同樣需要 —— 要進控制台就得知道 IP 和 PIN。原本連上後只顯示
/// "Telegram + Gemini ready",使用者看了不知道下一步該做什麼,
/// 也沒有別的地方查得到 IP(除非他已經綁好 Telegram 能問 /status,
/// 但那正是還沒綁定的人最需要這個畫面的時候)。
pub fn show_connected_screen(pins: &LcdPins, ssid: &str, ip: &str, pin: &str) -> Result<()> {
    with_lcd_display(pins, LcdScene::Connected { ssid, ip, pin })?;
    info!("LCD 已連線畫面: SSID={} IP={} PIN={}", ssid, ip, pin);
    Ok(())
}

/// 配網畫面與已連線畫面共用同一套版面 —— 兩者要傳達的是同一類東西
/// (在哪、怎麼進來、密碼多少),長得一樣使用者才不用重新學一次。
fn render_info_screen<D>(
    display: &mut D,
    ssid: &str,
    line1_label: &str,
    line1_value: &str,
    line2_label: &str,
    pin: &str,
) -> Result<()>
where
    D: DrawTarget<Color = Rgb565> + OriginDimensions,
{
    const BG: Rgb565 = Rgb565::new(27, 56, 30); // 淺色底,室內光線下比深色好讀
    const INK: Rgb565 = Rgb565::new(3, 7, 9);
    const HINT: Rgb565 = Rgb565::new(12, 24, 22);
    const ACCENT: Rgb565 = Rgb565::new(24, 30, 6);

    display.clear(BG).map_err(|_| anyhow!("lcd clear failed"))?;
    let size = display.bounding_box().size;
    let w = size.width as i32;
    let h = size.height as i32;

    // 頂端色帶:熱點名稱 —— 使用者第一個動作就是去手機 WiFi 清單找它
    const TOP_H: i32 = 26;
    RoundedRectangle::with_equal_corners(
        Rectangle::new(Point::new(4, 2), Size::new((w - 8) as u32, TOP_H as u32)),
        Size::new(8, 8),
    )
    .into_styled(PrimitiveStyleBuilder::new().fill_color(ACCENT).build())
    .draw(display)
    .map_err(|_| anyhow!("lcd bar draw failed"))?;
    let ssid_w = cjk_text_width(ssid, 1);
    draw_cjk_text(display, ssid, Point::new(((w - ssid_w) / 2).max(6), 7), INK)?;

    // 娜娜本人 —— 配網畫面一樣以她為主體。開箱第一眼看到的是這台裝置的「臉」,
    // 不是一頁說明文字;設定資訊繞著她排,不要把她擠掉。
    const BOTTOM_H: i32 = 62;
    let face_x = w / 2 - (FACE_W as i32) / 2;
    let face_y = TOP_H + 4 + ((h - TOP_H - 4 - BOTTOM_H) - (FACE_H as i32)) / 2;
    blit_face_bitmap(
        display,
        FaceImage::Smile.bytes(),
        FACE_W,
        Point::new(face_x, face_y.max(TOP_H + 4)),
    )?;

    // 底部:要照著打進手機的兩樣東西。行高 20 讓兩行剛好塞進色塊。
    let bar_y = h - BOTTOM_H;
    RoundedRectangle::with_equal_corners(
        Rectangle::new(Point::new(4, bar_y), Size::new((w - 8) as u32, (BOTTOM_H - 4) as u32)),
        Size::new(10, 10),
    )
    .into_styled(
        PrimitiveStyleBuilder::new()
            .fill_color(Rgb565::new(30, 60, 31))
            .stroke_color(ACCENT)
            .stroke_width(1)
            .build(),
    )
    .draw(display)
    .map_err(|_| anyhow!("lcd info bar failed"))?;

    let line1 = format!("{} {}", line1_label, line1_value);
    let line2 = format!("{} {}", line2_label, pin);
    let w1 = cjk_text_width(&line1, 1);
    let w2 = cjk_text_width(&line2, 1);
    draw_cjk_text(display, &line1, Point::new(((w - w1) / 2).max(8), bar_y + 8), INK)?;
    draw_cjk_text(display, &line2, Point::new(((w - w2) / 2).max(8), bar_y + 30), HINT)?;

    Ok(())
}

/// 沒人講話時定期換一張臉,讓她看起來是活的而不是一張定格照片。
/// 走的是跟一般畫面同一套隨機分布(30/30/30/10 + 比愛心冷卻),
/// 副標題輪替幾句待機文字,免得整片螢幕完全沒動靜。
///
/// 「閒」的判準是距離**上次任何一次畫面更新**夠久,不是距離上次待機換臉 ——
/// 否則使用者剛收到回覆,幾秒後就被待機畫面洗掉。
pub fn show_idle_face(pins: &LcdPins, tick: u32) -> Result<()> {
    const IDLE_LINES: [&str; 4] = [
        "Ready - just say nana",
        "Standing by",
        "Ready when you are",
        "Listening for nana",
    ];
    let last = LAST_DRAW_AT.load(core::sync::atomic::Ordering::Relaxed);
    if last != 0 && board_uptime_secs().saturating_sub(last) < IDLE_FACE_INTERVAL_SECS {
        return Ok(()); // 螢幕上還是新的內容,別蓋掉
    }
    draw_lcd_scene(pins, "NANA", IDLE_LINES[(tick as usize) % IDLE_LINES.len()])
}

pub fn show_reply_screen(pins: &LcdPins, text: &str) -> Result<()> {
    let clean = text.replace('\n', " ");
    let summary: String = clean.chars().take(36).collect();
    draw_lcd_scene(pins, "NANA", &summary)
}

pub fn draw_lcd_scene(pins: &LcdPins, headline: &str, subtitle: &str) -> Result<()> {
    with_lcd_display(pins, LcdScene::Content { headline, subtitle })?;
    info!("LCD scene rendered: {} | {}", headline, subtitle);
    Ok(())
}

/// 要畫哪一種畫面。
///
/// 用列舉而不是傳閉包,是因為 `DrawTarget` 不是 object-safe(關聯型別 + 泛型方法),
/// 做不出 `&mut dyn DrawTarget`;而有無 reset pin 兩條路徑的 display 是不同的
/// 具體型別,普通閉包也吃不下兩種。列舉配上一個泛型 `render` 方法就兩邊都能用。
enum LcdScene<'a> {
    Content { headline: &'a str, subtitle: &'a str },
    Provisioning { ssid: &'a str, url: &'a str, pin: &'a str },
    ProvisioningSecured { ssid: &'a str, ap_pass: &'a str, pin: &'a str },
    Connected { ssid: &'a str, ip: &'a str, pin: &'a str },
}

impl<'a> LcdScene<'a> {
    fn render<D>(&self, display: &mut D) -> Result<()>
    where
        D: DrawTarget<Color = Rgb565> + OriginDimensions,
    {
        match self {
            LcdScene::Content { headline, subtitle } => {
                render_lcd_content(display, headline, subtitle)
            }
            LcdScene::Provisioning { ssid, url, pin } => {
                render_info_screen(display, ssid, "手機開這個網址", url, "密碼", pin)
            }
            LcdScene::ProvisioningSecured { ssid, ap_pass, pin } => {
                render_info_screen(display, ssid, "熱點密碼", ap_pass, "登入密碼", pin)
            }
            LcdScene::Connected { ssid, ip, pin } => {
                render_info_screen(display, ssid, "控制台", ip, "密碼", pin)
            }
        }
    }
}

/// 開好 SPI + ST7789,把 display 交給 `scene` 畫,畫完自動收尾。
///
/// 面板初始化參數(色序/反相)是這片面板的批次特性,寫錯的徵兆是白變黑、紅藍互換。
/// 抽成一個地方,才不會兩種畫面各留一份設定、改了一邊忘了另一邊。
fn with_lcd_display(pins: &LcdPins, scene: LcdScene<'_>) -> Result<()> {
    ensure_pin(pins.dc, "lcd.dc")?;
    ensure_pin(pins.bl, "lcd.bl")?;
    ensure_pin(pins.sclk, "lcd.sclk")?;
    ensure_pin(pins.sda, "lcd.sda")?;

    if is_zhengchen_cam_lcd(pins) {
        if let Err(err) = configure_board_io_expander_bit(0, false) {
            warn!("LCD power enable via PCA9557 failed: {}", err);
        }
    }

    let peripherals = unsafe { Peripherals::steal() };
    let spi = peripherals.spi2;

    let dc = PinDriver::output(unsafe { AnyOutputPin::steal(pins.dc as u8) })?;
    let sclk = unsafe { AnyIOPin::steal(pins.sclk as u8) };
    let sda = unsafe { AnyIOPin::steal(pins.sda as u8) };

    let mut delay = Ets;
    let (panel_width, panel_height) = normalize_st7789_size(pins.width, pins.height);
    let config = spi::config::Config::new()
        .baudrate(26.MHz().into())
        .data_mode(MODE_0);

    let device = SpiDeviceDriver::new_single(
        spi,
        sclk,
        sda,
        Option::<AnyIOPin>::None,
        if pins.cs >= 0 {
            Some(unsafe { AnyIOPin::steal(pins.cs as u8) })
        } else {
            Option::<AnyIOPin>::None
        },
        &SpiDriverConfig::new(),
        &config,
    )?;

    let di = SPIInterface::new(device, dc);
    if pins.rst >= 0 {
        let rst = PinDriver::output(unsafe { AnyOutputPin::steal(pins.rst as u8) })?;
        let mut display = Builder::new(ST7789, di)
            .display_size(panel_width, panel_height)
            .orientation(Orientation::new())
            .color_order(ColorOrder::Rgb)
            .invert_colors(ColorInversion::Inverted)
            .reset_pin(rst)
            .init(&mut delay)
            .map_err(|_| anyhow!("st7789 init failed"))?;
        set_lcd_backlight(pins)?;
        scene.render(&mut display)?;
    } else {
        let mut display = Builder::new(ST7789, di)
            .display_size(panel_width, panel_height)
            .orientation(Orientation::new())
            .color_order(ColorOrder::Rgb)
            .invert_colors(ColorInversion::Inverted)
            .init(&mut delay)
            .map_err(|_| anyhow!("st7789 init failed"))?;
        set_lcd_backlight(pins)?;
        scene.render(&mut display)?;
    }

    LAST_DRAW_AT.store(board_uptime_secs().max(1), core::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// 把一張預先轉檔好的 RGB565 點陣圖貼到螢幕上 — 這是「播放照片」而不是
/// 「即時畫圖」,畫質不受向量繪圖天花板限制,可以是任何等級的插畫。
///
/// `bytes` 必須是 `tools/png_to_rgb565.py` 轉出的格式:
/// 每像素 2 bytes、大端序(big-endian)、無檔頭,大小需等於 `width * height * 2`。
///
/// 使用方式(等真的有素材時,在 render_lcd_content 依 expression 選對應陣列):
/// ```ignore
/// static FACE_HAPPY: &[u8] = include_bytes!("../assets/faces/happy.rgb565");
/// blit_face_bitmap(display, FACE_HAPPY, 176, Point::new(face_cx - 88, face_cy - 110))?;
/// ```
pub fn blit_face_bitmap<D>(display: &mut D, bytes: &[u8], width: u32, pos: Point) -> Result<()>
where
    D: DrawTarget<Color = Rgb565>,
{
    let raw = ImageRaw::<Rgb565>::new(bytes, width);
    Image::new(&raw, pos)
        .draw(display)
        .map_err(|_| anyhow!("lcd bitmap blit failed"))?;
    Ok(())
}

fn render_lcd_content<D>(display: &mut D, headline: &str, subtitle: &str) -> Result<()>
where
    D: DrawTarget<Color = Rgb565> + OriginDimensions,
{
    let (headline, subtitle, expression) = parse_lcd_expression(headline, subtitle);
    let state = classify_lcd_state(&headline, &subtitle);
    // 插畫底圖上場後,per-expression 的臉部色票(眼白/瞳孔/腮紅/嘴色...)不再用來
    // 即時畫圖形,只留 bg/face_stroke(狀態列用)與文字色。expression 仍會被
    // parse_lcd_expression 用來從回覆文字裡把 [FACE:xxx] 標籤清掉,故保留參數。
    let (bg, _, face_stroke, _, _, _, _, _, _, body_color, meta_color, _, state_label) =
        emoji_palette(state, expression);

    // 只剩狀態列還用得到 ASCII 字型(NANA / BOOT / READY 這類固定英文標籤);
    // 回覆內容改走自帶的中文點陣字型,見下面的 draw_cjk_text。
    let meta_style = MonoTextStyleBuilder::new()
        .font(&FONT_6X10)
        .text_color(meta_color)
        .build();

    display.clear(bg).map_err(|_| anyhow!("lcd clear failed"))?;
    let size = display.bounding_box().size;
    let w = size.width as i32;
    let h = size.height as i32;

    // ===== Top status bar =====
    RoundedRectangle::with_equal_corners(
        Rectangle::new(Point::new(4, 2), Size::new((w - 8) as u32, 20)),
        Size::new(10, 10),
    )
    .into_styled(PrimitiveStyleBuilder::new().fill_color(face_stroke).build())
    .draw(display)
    .map_err(|_| anyhow!("lcd bar draw failed"))?;
    Text::with_alignment("NANA", Point::new(32, 16), meta_style, Alignment::Center)
        .draw(display).map_err(|_| anyhow!("lcd text failed"))?;
    Text::with_alignment(state_label, Point::new(w / 2, 16), meta_style, Alignment::Center)
        .draw(display).map_err(|_| anyhow!("lcd text failed"))?;
    // Status dot
    Circle::new(Point::new(8, 8), 7)
        .into_styled(PrimitiveStyleBuilder::new().fill_color(Rgb565::new(10, 40, 16)).build())
        .draw(display).map_err(|_| anyhow!("lcd dot failed"))?;

    // ===== 四張插畫表情圖 =====
    // 全部只「貼圖」不「畫圖形」:在插畫臉上疊向量橢圓/弧線怎麼調都很突兀
    // (前一版的手繪笑臉就是被這樣退回的),所以表情一律換整張圖。
    // 四張共用同一個裁切構圖 → 切換表情時頭不會在螢幕上跳位置。
    let face_x = w / 2 - (FACE_W as i32) / 2;
    let face_y = 26 + ((h - 26 - 58) - (FACE_H as i32)) / 2;
    let face_pos = Point::new(face_x, face_y);

    let face = pick_face(expression);
    blit_face_bitmap(display, face.bytes(), FACE_W, face_pos)?;

    // 講話動畫:播 TTS 時在「閉嘴」跟「開嘴」兩張真圖之間輪替 —— 這是用畫好的
    // 插畫做嘴型,不是在臉上補畫嘴。比愛心那張沒有配對圖(雙手會閃爍)故不動。
    if matches!(state, LcdState::Speaking) {
        if let Some(alt) = face.talk_partner() {
            let cycles = (subtitle.chars().count() / 8).clamp(3, 8);
            for _ in 0..cycles {
                blit_face_bitmap(display, alt.bytes(), FACE_W, face_pos)?;
                std::thread::sleep(Duration::from_millis(150));
                blit_face_bitmap(display, face.bytes(), FACE_W, face_pos)?;
                std::thread::sleep(Duration::from_millis(110));
            }
        }
    }

    // ===== Bottom info bar =====
    let bar_h: i32 = if h >= 300 { 56 } else { 44 };
    RoundedRectangle::with_equal_corners(
        Rectangle::new(Point::new(4, h - bar_h - 2), Size::new((w - 8) as u32, bar_h as u32)),
        Size::new(12, 12),
    )
    .into_styled(PrimitiveStyleBuilder::new()
        .fill_color(Rgb565::new(
            bg.r().saturating_add(2).min(31),
            bg.g().saturating_add(4).min(63),
            bg.b().saturating_add(2).min(31),
        ))
        .stroke_color(face_stroke)
        .stroke_width(1)
        .build())
    .draw(display).map_err(|_| anyhow!("lcd info bar failed"))?;

    // 回覆內容用自帶的 16x16 點陣字型畫 —— embedded-graphics 內建字型是純 ASCII,
    // 中文一律變成 '?',所以這條路不能用 Text::with_alignment。
    // headline 固定是 "NANA",上面狀態列已經有了,這裡整條讓給回覆文字。
    let text_x = 12;
    let text_w = w - text_x * 2;
    let first_y = h - bar_h + 4;
    for (index, line) in wrap_cjk_text(&subtitle, text_w, 2).iter().enumerate() {
        draw_cjk_text(
            display,
            line,
            Point::new(text_x, first_y + index as i32 * (CJK_CELL + 2)),
            body_color,
        )?;
    }

    Ok(())
}

/// 自帶的 16x16 中文點陣字型(tools/build_cjk_font.py 產生)。
/// 格式:u16 字數 + 已排序的 u16 碼位表 + 每字 32 bytes 點陣(16 列 x 2 bytes,MSB 在左)。
static CJK_FONT: &[u8] = include_bytes!("../assets/font16.bin");
const CJK_CELL: i32 = 16;

fn cjk_font_count() -> usize {
    u16::from_le_bytes([CJK_FONT[0], CJK_FONT[1]]) as usize
}

/// 半形前進 8 px、全形 16 px。字形一律存 16x16,半形只用左半邊。
fn cjk_advance(ch: char) -> i32 {
    if (ch as u32) < 0x80 { 8 } else { CJK_CELL }
}

/// 二分搜尋碼位表拿字形。找不到就回 None(該字不畫,不是畫成問號)。
fn cjk_glyph(ch: char) -> Option<&'static [u8]> {
    let code = u16::try_from(ch as u32).ok()?;
    let count = cjk_font_count();
    let idx_off = 2;
    let gly_off = idx_off + count * 2;
    let (mut lo, mut hi) = (0usize, count);
    while lo < hi {
        let mid = (lo + hi) / 2;
        let p = idx_off + mid * 2;
        let v = u16::from_le_bytes([CJK_FONT[p], CJK_FONT[p + 1]]);
        if v < code { lo = mid + 1 } else { hi = mid }
    }
    if lo >= count {
        return None;
    }
    let p = idx_off + lo * 2;
    if u16::from_le_bytes([CJK_FONT[p], CJK_FONT[p + 1]]) != code {
        return None;
    }
    let g = gly_off + lo * 32;
    Some(&CJK_FONT[g..g + 32])
}

/// 畫一行字(支援中英混排)。回傳畫完後的總寬度 px。
fn draw_cjk_text<D>(display: &mut D, text: &str, pos: Point, color: Rgb565) -> Result<i32>
where
    D: DrawTarget<Color = Rgb565>,
{
    draw_cjk_text_scaled(display, text, pos, color, 1)
}

/// 同上,但每個點放大成 scale x scale 的方塊。
/// 配網畫面的網址/PIN 用 scale=2(等於 32px 字),長輩不用瞇眼睛看。
fn draw_cjk_text_scaled<D>(
    display: &mut D,
    text: &str,
    pos: Point,
    color: Rgb565,
    scale: i32,
) -> Result<i32>
where
    D: DrawTarget<Color = Rgb565>,
{
    let scale = scale.max(1);
    let mut x = pos.x;
    for ch in text.chars() {
        if let Some(g) = cjk_glyph(ch) {
            // 一次收一個字的點,避免整行幾千個 Pixel 一次進 Vec
            let mut pixels = Vec::with_capacity(96 * (scale * scale) as usize);
            for row in 0..16usize {
                let bits = u16::from_le_bytes([g[row * 2], g[row * 2 + 1]]);
                if bits == 0 {
                    continue;
                }
                for col in 0..16usize {
                    if bits & (1 << (15 - col)) == 0 {
                        continue;
                    }
                    for dy in 0..scale {
                        for dx in 0..scale {
                            pixels.push(Pixel(
                                Point::new(
                                    x + col as i32 * scale + dx,
                                    pos.y + row as i32 * scale + dy,
                                ),
                                color,
                            ));
                        }
                    }
                }
            }
            display
                .draw_iter(pixels)
                .map_err(|_| anyhow!("lcd cjk draw failed"))?;
        }
        x += cjk_advance(ch) * scale;
    }
    Ok(x - pos.x)
}

/// 一行字用某個放大倍率畫出來會有多寬。
fn cjk_text_width(text: &str, scale: i32) -> i32 {
    text.chars().map(cjk_advance).sum::<i32>() * scale.max(1)
}

/// 依「顯示寬度」斷行 —— 中文沒有空白可以拆,不能沿用以空白斷詞的做法。
/// 超過行數上限就在最後一行結尾補「…」,讓人知道還有沒顯示完的內容。
fn wrap_cjk_text(text: &str, max_px: i32, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_px = 0;
    for ch in text.chars() {
        if ch == '\n' {
            lines.push(std::mem::take(&mut cur));
            cur_px = 0;
            if lines.len() >= max_lines {
                break;
            }
            continue;
        }
        let adv = cjk_advance(ch);
        if cur_px + adv > max_px {
            lines.push(std::mem::take(&mut cur));
            cur_px = 0;
            if lines.len() >= max_lines {
                break;
            }
        }
        cur.push(ch);
        cur_px += adv;
    }
    if lines.len() < max_lines && !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::from("待機中"));
    } else if text.chars().count() > lines.iter().map(|l| l.chars().count()).sum::<usize>() {
        if let Some(last) = lines.last_mut() {
            while !last.is_empty()
                && last.chars().map(cjk_advance).sum::<i32>() + CJK_CELL > max_px
            {
                last.pop();
            }
            last.push('…');
        }
    }
    lines
}

#[derive(Clone, Copy)]
enum LcdState {
    Boot,
    Ready,
    Wake,
    Speaking,
    Error,
    Message,
}

#[derive(Clone, Copy)]
enum LcdExpression {
    Neutral,
    Smile,
    Happy,
    Wink,
    Love,
    Thinking,
    Sad,
    Surprise,
    Angry,
}

/// 螢幕上實際貼的四張插畫。表情語意有九種、圖只有四張,多對一收斂在
/// `for_expression()`;沒有圖的心情(生氣/難過/思考…)一律回常規臉,
/// 寧可少變化也不要貼一張跟語氣明顯不搭的圖。
#[derive(Clone, Copy)]
pub enum FaceImage {
    Neutral,
    Smile,
    Happy,
    Love,
}

/// 四張圖共用的邊長。改這裡要同步跑 `python tools/build_face_assets.py --size N`。
const FACE_W: u32 = 216;
const FACE_H: u32 = 216;

impl FaceImage {
    fn bytes(self) -> &'static [u8] {
        match self {
            FaceImage::Neutral => include_bytes!("../assets/faces/neutral.rgb565"),
            FaceImage::Smile => include_bytes!("../assets/faces/smile.rgb565"),
            FaceImage::Happy => include_bytes!("../assets/faces/happy.rgb565"),
            FaceImage::Love => include_bytes!("../assets/faces/love.rgb565"),
        }
    }

    /// 講話動畫要輪替的另一張圖 —— 挑「嘴巴開合狀態相反」的那張:
    /// 常規是微張嘴、微笑是抿嘴、很開心是大笑張嘴,兩兩交錯就有講話感。
    /// 比愛心那張手擋在臉前,換圖會讓雙手閃爍,所以不配對。
    fn talk_partner(self) -> Option<FaceImage> {
        match self {
            FaceImage::Neutral => Some(FaceImage::Smile),
            FaceImage::Smile => Some(FaceImage::Neutral),
            FaceImage::Happy => Some(FaceImage::Smile),
            FaceImage::Love => None,
        }
    }

    fn for_expression(expr: LcdExpression) -> FaceImage {
        match expr {
            LcdExpression::Smile | LcdExpression::Wink => FaceImage::Smile,
            LcdExpression::Happy => FaceImage::Happy,
            LcdExpression::Love => FaceImage::Love,
            // 沒有專屬插畫的心情 → 常規臉
            LcdExpression::Neutral
            | LcdExpression::Thinking
            | LcdExpression::Sad
            | LcdExpression::Surprise
            | LcdExpression::Angry => FaceImage::Neutral,
        }
    }
}

/// AI 沒指定表情時的預設分布:常規 30% / 微笑 30% / 很開心 30% / 比愛心 10%。
/// 比愛心刻意壓低 —— 動作太大,每次都比就不特別了。
fn weighted_default_expression() -> LcdExpression {
    let roll = (unsafe { esp_idf_sys::esp_random() } % 100) as u8;
    match roll {
        0..=29 => LcdExpression::Neutral,
        30..=59 => LcdExpression::Smile,
        60..=89 => LcdExpression::Happy,
        _ => LcdExpression::Love,
    }
}

/// 上次比愛心的開機秒數(0=還沒比過)。
static LOVE_LAST_SHOWN: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// 比愛心的冷卻時間。10 分鐘內只會出現一次,不管誰要求。
const LOVE_COOLDOWN_SECS: u32 = 600;

fn board_uptime_secs() -> u32 {
    (unsafe { esp_idf_sys::esp_timer_get_time() } / 1_000_000) as u32
}

/// 決定這次真正要貼哪張圖,並強制執行「比愛心是稀有動作」這條規則。
///
/// 30/30/30/10 那個機率只管「AI 沒指定表情」的情況;AI 自己指定時完全不受約束,
/// 而語言模型很容易黏在同一個標籤上連發好幾則 —— 一直比愛心就不特別了。
/// 所以這裡加一道硬性冷卻:冷卻中不管誰要求比愛心,一律降級成大笑。
fn pick_face(expr: LcdExpression) -> FaceImage {
    let face = FaceImage::for_expression(expr);
    if !matches!(face, FaceImage::Love) {
        return face;
    }
    let now = board_uptime_secs();
    let last = LOVE_LAST_SHOWN.load(core::sync::atomic::Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < LOVE_COOLDOWN_SECS {
        return FaceImage::Happy;
    }
    LOVE_LAST_SHOWN.store(now.max(1), core::sync::atomic::Ordering::Relaxed);
    face
}

fn normalize_lcd_expression(raw: &str) -> Option<LcdExpression> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "neutral" | "calm" | "staticstate" => Some(LcdExpression::Neutral),
        "smile" | "gentle" | "soft" => Some(LcdExpression::Smile),
        "happy" | "laugh" | "excited" | "joy" => Some(LcdExpression::Happy),
        "wink" | "playful" => Some(LcdExpression::Wink),
        "love" | "heart" => Some(LcdExpression::Love),
        "thinking" | "think" | "confused" | "buxue" => Some(LcdExpression::Thinking),
        "sad" | "shy" | "down" => Some(LcdExpression::Sad),
        "surprise" | "surprised" | "scare" | "shocked" => Some(LcdExpression::Surprise),
        "angry" | "anger" | "mad" => Some(LcdExpression::Angry),
        _ => None,
    }
}

fn strip_face_tag(text: &str) -> (String, Option<LcdExpression>) {
    let mut clean = text.to_string();
    let mut expression = None;
    while let Some(start) = clean.find("[FACE:") {
        if let Some(end) = clean[start..].find(']') {
            let tag = &clean[start + 6..start + end];
            if let Some(parsed) = normalize_lcd_expression(tag) {
                expression = Some(parsed);
            }
            clean = format!("{}{}", &clean[..start], clean[start + end + 1..].trim_start());
        } else {
            break;
        }
    }
    (clean.trim().to_string(), expression)
}

fn default_expression_for_state(state: LcdState) -> LcdExpression {
    match state {
        // 開機/喚醒/出錯的臉是固定的 —— 這些畫面在講「裝置現在什麼狀態」,
        // 隨機換表情只會讓人看不懂。
        LcdState::Boot => LcdExpression::Thinking,
        LcdState::Wake => LcdExpression::Surprise,
        LcdState::Error => LcdExpression::Sad,
        // 對話類畫面才吃 30/30/30/10 的隨機分布,讓她看起來有情緒起伏
        LcdState::Ready | LcdState::Speaking | LcdState::Message => weighted_default_expression(),
    }
}

fn parse_lcd_expression(headline: &str, subtitle: &str) -> (String, String, LcdExpression) {
    let (headline, head_expression) = strip_face_tag(headline);
    let (subtitle, sub_expression) = strip_face_tag(subtitle);
    let state = classify_lcd_state(&headline, &subtitle);
    let expression = sub_expression
        .or(head_expression)
        .unwrap_or_else(|| default_expression_for_state(state));
    (headline, subtitle, expression)
}

fn classify_lcd_state(headline: &str, subtitle: &str) -> LcdState {
    let headline_upper = headline.to_ascii_uppercase();
    let subtitle_upper = subtitle.to_ascii_uppercase();
    if headline_upper.contains("ERROR") || subtitle_upper.contains("ERROR") || subtitle_upper.contains("FAILED") {
        LcdState::Error
    } else if headline_upper.contains("SPEAK") || subtitle_upper.contains("TTS") {
        LcdState::Speaking
    } else if subtitle_upper.contains("WAKE") {
        LcdState::Wake
    } else if subtitle_upper.contains("BOOT") {
        LcdState::Boot
    } else if subtitle_upper.contains("READY") {
        LcdState::Ready
    } else {
        LcdState::Message
    }
}

fn emoji_palette(
    state: LcdState,
    expr: LcdExpression,
) -> (Rgb565, Rgb565, Rgb565, Rgb565, Rgb565, Rgb565, Rgb565, Rgb565, Rgb565, Rgb565, Rgb565, Rgb565, &'static str) {
    // 淺色可愛(kawaii)風主題 — 柔和粉彩背景 + 深色文字,取代舊版深色駭客風。
    // State → background, bar/stroke(每個狀態換一種柔和色調), label。
    // 文字統一用深墨色(下面 INK_*),不隨狀態變,確保任何背景都讀得清楚。
    const INK_TITLE: Rgb565 = Rgb565::new(4, 8, 10);   // 深墨(近黑,略帶暖色)
    const INK_BODY: Rgb565 = Rgb565::new(9, 17, 16);   // 深灰墨
    const INK_META: Rgb565 = Rgb565::new(15, 26, 22);  // 中灰墨(次要文字)
    let (bg, stroke, label) = match state {
        LcdState::Boot     => (Rgb565::new(27, 52, 31), Rgb565::new(18, 28, 24), "BOOT"),   // 淡薰衣草
        LcdState::Ready    => (Rgb565::new(27, 58, 26), Rgb565::new(14, 34, 18), "READY"),  // 淡薄荷
        LcdState::Wake     => (Rgb565::new(25, 54, 31), Rgb565::new(12, 26, 30), "WAKE"),   // 淡天藍
        LcdState::Speaking => (Rgb565::new(31, 56, 24), Rgb565::new(26, 34, 10), "VOICE"),  // 淡奶油黃
        LcdState::Error    => (Rgb565::new(31, 46, 26), Rgb565::new(28, 16, 14), "ERROR"),  // 淡玫瑰
        LcdState::Message  => (Rgb565::new(26, 56, 31), Rgb565::new(12, 28, 28), "LIVE"),   // 淡青
    };
    // Expression → face(統一暖白膚色)、eye_white、pupil(深藏青)、iris(隨心情變色)、
    // accent(特效元素:愛心/思考泡泡/怒氣記號)、blush(腮紅)、mouth(柔和深玫瑰)。
    const FACE_SKIN: Rgb565 = Rgb565::new(31, 61, 28);   // 暖白/象牙色
    const EYE_WHITE: Rgb565 = Rgb565::new(31, 63, 31);   // 純白
    const PUPIL_INK: Rgb565 = Rgb565::new(2, 4, 7);      // 深藏青(瞳孔)
    const MOUTH_ROSE: Rgb565 = Rgb565::new(13, 8, 10);   // 柔和深玫瑰(嘴線,非純黑)
    let (iris, accent, blush) = match expr {
        LcdExpression::Neutral  => (Rgb565::new(10, 30, 27), Rgb565::new(12, 38, 26), Rgb565::new(31, 40, 30)),
        LcdExpression::Smile    => (Rgb565::new(16, 36, 20), Rgb565::new(26, 34, 14), Rgb565::new(31, 38, 26)),
        LcdExpression::Happy    => (Rgb565::new(24, 34, 6),  Rgb565::new(31, 30, 8),  Rgb565::new(31, 36, 24)),
        LcdExpression::Wink     => (Rgb565::new(8, 40, 28),  Rgb565::new(20, 42, 20), Rgb565::new(31, 38, 26)),
        LcdExpression::Love     => (Rgb565::new(28, 14, 20), Rgb565::new(31, 12, 18), Rgb565::new(31, 32, 26)),
        LcdExpression::Thinking => (Rgb565::new(18, 14, 28), Rgb565::new(16, 34, 31), Rgb565::new(30, 40, 28)),
        LcdExpression::Sad      => (Rgb565::new(14, 32, 31), Rgb565::new(10, 26, 31), Rgb565::new(26, 36, 30)),
        LcdExpression::Surprise => (Rgb565::new(8, 44, 31),  Rgb565::new(31, 26, 8),  Rgb565::new(31, 38, 28)),
        LcdExpression::Angry    => (Rgb565::new(28, 16, 6),  Rgb565::new(31, 10, 8),  Rgb565::new(31, 26, 18)),
    };
    (bg, FACE_SKIN, stroke, EYE_WHITE, PUPIL_INK, accent, blush, MOUTH_ROSE, INK_TITLE, INK_BODY, INK_META, iris, label)
}


pub fn play_test_tone(pins: &AudioPins) -> Result<()> {
    ensure_pin(pins.bclk, "audio.bclk")?;
    ensure_pin(pins.ws, "audio.ws")?;
    ensure_pin(pins.dout, "audio.dout")?;

    if is_zhengchen_cam_audio(pins) {
        enable_zhengchen_cam_speaker(pins.sample_rate)?;
        let _ = configure_board_io_expander_bit(1, true);
    }

    let peripherals = unsafe { Peripherals::steal() };
    let cfg = StdConfig::philips(pins.sample_rate, DataBitWidth::Bits16);
    let mut i2s = I2sDriver::<I2sTx>::new_std_tx(
        peripherals.i2s0,
        &cfg,
        unsafe { AnyIOPin::steal(pins.bclk as u8) },
        unsafe { AnyIOPin::steal(pins.dout as u8) },
        if pins.mclk >= 0 {
            Some(unsafe { AnyIOPin::steal(pins.mclk as u8) })
        } else {
            AnyIOPin::none()
        },
        unsafe { AnyIOPin::steal(pins.ws as u8) },
    )?;

    let mut frames = Vec::new();
    for freq in [392.0f32, 493.88f32, 587.33f32] {
        append_sine(&mut frames, pins.sample_rate, freq, 120, 0.12);
        append_silence(&mut frames, pins.sample_rate, 40);
    }

    i2s.tx_enable()?;
    i2s.write_all(&frames, 1000)?;
    info!("Audio tone test played: {} bytes", frames.len());
    Ok(())
}

pub fn play_pcm16(pins: &AudioPins, pcm: &[i16], sample_rate: u32, channels: usize) -> Result<usize> {
    ensure_pin(pins.bclk, "audio.bclk")?;
    ensure_pin(pins.ws, "audio.ws")?;
    ensure_pin(pins.dout, "audio.dout")?;

    if is_zhengchen_cam_audio(pins) {
        enable_zhengchen_cam_speaker(sample_rate)?;
        let _ = configure_board_io_expander_bit(1, true);
    }

    if pcm.is_empty() {
        bail!("pcm buffer is empty");
    }

    let peripherals = unsafe { Peripherals::steal() };
    let cfg = StdConfig::philips(sample_rate, DataBitWidth::Bits16);
    let mut i2s = I2sDriver::<I2sTx>::new_std_tx(
        peripherals.i2s0,
        &cfg,
        unsafe { AnyIOPin::steal(pins.bclk as u8) },
        unsafe { AnyIOPin::steal(pins.dout as u8) },
        if pins.mclk >= 0 {
            Some(unsafe { AnyIOPin::steal(pins.mclk as u8) })
        } else {
            AnyIOPin::none()
        },
        unsafe { AnyIOPin::steal(pins.ws as u8) },
    )?;

    // 分塊送出,不要把整段話一次展開成位元組陣列。
    //
    // 原本是 `Vec::with_capacity(pcm.len() * 4)` 一次配足 —— 單聲道要複製成雙聲道,
    // 每個 sample 佔 4 bytes。一句 30 秒的回覆(718272 samples)就是 2.87MB 的**單一
    // 連續配置**,而那時記憶體裡還有解碼出來的 PCM(1.44MB)與 TTS 快取的 MP3。
    // PSRAM 有 8MB 但會碎片化,配不出這麼大的連續區塊就 abort,裝置直接重開:
    //
    //     memory allocation of 2873088 bytes failed
    //     abort() was called at PC 0x420909a6
    //
    // 這是使用者長期回報「有時候自己重開機」的根因,而且愈長的回覆愈容易踩到。
    // 改成固定大小的緩衝重複使用,記憶體用量與回覆長度**無關**。
    const CHUNK_SAMPLES: usize = 512; // 512 samples -> 最多 2KB,遠低於任何碎片門檻
    let mut buf: Vec<u8> = Vec::with_capacity(CHUNK_SAMPLES * 4);

    i2s.tx_enable()?;
    let mut written = 0usize;
    for chunk in pcm.chunks(CHUNK_SAMPLES) {
        buf.clear();
        if channels <= 1 {
            for sample in chunk {
                let bytes = sample.to_le_bytes();
                buf.extend_from_slice(&bytes); // L
                buf.extend_from_slice(&bytes); // R
            }
        } else {
            for sample in chunk {
                buf.extend_from_slice(&sample.to_le_bytes());
            }
        }
        // 第二個參數是 FreeRTOS tick 不是毫秒:100Hz 下 2000 tick = 20 秒,
        // 一塊 2KB 的資料卡 20 秒才逾時太久。100 tick = 1 秒足夠且失敗得快。
        i2s.write_all(&buf, 100)?;
        written += buf.len();
        // 播放期間必須持續餵看門狗。write_all 會一路阻塞到 DMA 排空,一段 34 秒的
        // 回覆(824832 samples @24kHz)就讓主任務整整 34 秒不回頭 —— 加上前面的
        // 下載與解碼,累計超過 60 秒的 WDT 預算,裝置就被踢重開:
        //     E task_wdt: Task watchdog got triggered ... - main (CPU 0)
        // 分塊送出本來是為了修記憶體,這裡剛好也給了餵狗的落腳點。
        super::feed_watchdog();
    }
    info!("PCM audio played: {} samples @ {} Hz ({} ch), {} bytes streamed",
          pcm.len(), sample_rate, channels, written);
    Ok(written)
}

pub fn capture_mic_pcm(pins: &AudioPins, duration_ms: u32, max_samples: usize) -> Result<Vec<i16>> {
    ensure_pin(pins.mic_sck, "audio.mic_sck")?;
    ensure_pin(pins.mic_ws, "audio.mic_ws")?;
    ensure_pin(pins.mic_din, "audio.mic_din")?;

    let peripherals = unsafe { Peripherals::steal() };
    let cfg = StdConfig::philips(pins.sample_rate, DataBitWidth::Bits16);
    let mut i2s = I2sDriver::<I2sRx>::new_std_rx(
        peripherals.i2s1,
        &cfg,
        unsafe { AnyIOPin::steal(pins.mic_sck as u8) },
        unsafe { AnyIOPin::steal(pins.mic_din as u8) },
        if pins.mclk >= 0 {
            Some(unsafe { AnyIOPin::steal(pins.mclk as u8) })
        } else {
            AnyIOPin::none()
        },
        unsafe { AnyIOPin::steal(pins.mic_ws as u8) },
    )?;

    let deadline = Instant::now() + Duration::from_millis(duration_ms as u64);
    let mut buf = [0u8; 2048];
    let mut pcm = Vec::with_capacity(max_samples.min((pins.sample_rate as usize * duration_ms as usize / 1000) + 1024));

    i2s.rx_enable()?;
    while Instant::now() < deadline && pcm.len() < max_samples {
        let read = i2s.read(&mut buf, 100)?;
        for chunk in buf[..read].chunks_exact(2) {
            pcm.push(i16::from_le_bytes([chunk[0], chunk[1]]));
            if pcm.len() >= max_samples {
                break;
            }
        }
    }

    // philips_slot_default 是立體聲（L/R 交錯），板上只有一顆麥克風、只推一個聲道。
    // 原本把交錯流當連續單聲道用：一半樣本是死聲道的零，RMS 打對折、時間軸被拉成兩倍長，
    // 喚醒詞與語音轉寫都吃這個。這裡不猜麥克風接在 L 還是 R——算兩個聲道各自的能量，
    // 一邊明顯是死的就只留活的那邊；兩邊都有聲音（雙麥克風或驅動其實是單聲道）就維持原樣，
    // 所以不會有退化。
    let (mut e_even, mut e_odd) = (0u64, 0u64);
    for (i, s) in pcm.iter().enumerate() {
        let v = (*s as i64).unsigned_abs();
        if i % 2 == 0 { e_even += v } else { e_odd += v }
    }
    let (hi, lo) = if e_even >= e_odd { (e_even, e_odd) } else { (e_odd, e_even) };
    if hi > 0 && lo * 8 < hi {
        let keep_even = e_even >= e_odd;
        pcm = pcm.iter().enumerate()
            .filter(|(i, _)| (i % 2 == 0) == keep_even)
            .map(|(_, s)| *s)
            .collect();
        info!("mic: 立體聲交錯流只有 {} 聲道有訊號，已抽取", if keep_even { "L" } else { "R" });
    }

    if pcm.is_empty() {
        bail!("mic returned no pcm samples");
    }

    Ok(pcm)
}

pub fn capture_mic_snapshot(pins: &AudioPins, duration_ms: u32) -> Result<MicSnapshot> {
    let pcm = capture_mic_pcm(
        pins,
        duration_ms,
        pins.sample_rate as usize * duration_ms as usize / 1000 + 1024,
    )?;
    let mut energy: u64 = 0;
    let mut peak: i16 = 0;
    let samples = pcm.len();

    for sample in pcm {
        let abs = sample.saturating_abs();
        if abs > peak {
            peak = abs;
        }
        let signed = sample as i32;
        energy += (signed * signed) as u64;
    }

    if samples == 0 {
        bail!("mic returned no samples");
    }

    let rms = ((energy / samples as u64) as f64).sqrt() as u32;
    Ok(MicSnapshot { rms, peak, samples })
}

pub fn wake_probe(pins: &AudioPins, duration_ms: u32, threshold: u32) -> Result<bool> {
    let snapshot = capture_mic_snapshot(pins, duration_ms)?;
    Ok(snapshot.rms >= threshold)
}

fn ensure_pin(pin: i32, label: &str) -> Result<i32> {
    if pin < 0 {
        bail!("{} is not configured", label);
    }
    Ok(pin)
}

fn append_sine(out: &mut Vec<u8>, sample_rate: u32, freq: f32, duration_ms: u32, volume: f32) {
    let frame_count = (sample_rate as u64 * duration_ms as u64 / 1000) as usize;
    for idx in 0..frame_count {
        let phase = TAU * freq * (idx as f32 / sample_rate as f32);
        let sample = (phase.sin() * volume * i16::MAX as f32) as i16;
        out.extend_from_slice(&sample.to_le_bytes());
        out.extend_from_slice(&sample.to_le_bytes());
    }
}

fn append_silence(out: &mut Vec<u8>, sample_rate: u32, duration_ms: u32) {
    let frame_count = (sample_rate as u64 * duration_ms as u64 / 1000) as usize;
    for _ in 0..frame_count {
        out.extend_from_slice(&0i16.to_le_bytes());
        out.extend_from_slice(&0i16.to_le_bytes());
    }
}

fn is_zhengchen_cam_lcd(pins: &LcdPins) -> bool {
    pins.dc == 39 && pins.bl == 42 && pins.sclk == 41 && pins.sda == 40
}

fn is_zhengchen_cam_audio(pins: &AudioPins) -> bool {
    pins.bclk == 14 && pins.ws == 13 && pins.dout == 45 && pins.din == 12 && pins.mclk == 38
}

fn normalize_st7789_size(width: u16, height: u16) -> (u16, u16) {
    match (width, height) {
        (320, 240) => (240, 320),
        (w, h) if w > 240 && h <= 240 => (240, 320),
        (w, h) if w == 0 || h == 0 => (240, 320),
        (w, h) => (w.min(240), h.min(320)),
    }
}

fn set_lcd_backlight(pins: &LcdPins) -> Result<()> {
    let level = if is_zhengchen_cam_lcd(pins) { 0 } else { 1 };
    unsafe {
        gpio_reset_pin(pins.bl as i32);
        gpio_set_direction(pins.bl as i32, gpio_mode_t_GPIO_MODE_OUTPUT);
        gpio_set_level(pins.bl as i32, level);
    }
    info!("LCD backlight forced to level {} on GPIO{}", level, pins.bl);
    Ok(())
}

fn enable_zhengchen_cam_speaker(sample_rate: u32) -> Result<()> {
    info!("Audio bring-up: enabling PCA9557 output bit for speaker path");
    configure_board_io_expander_bit(1, true)?;

    info!("Audio bring-up: opening board I2C on GPIO{} / GPIO{}", BOARD_IO_SDA_PIN, BOARD_IO_SCL_PIN);
    let mut i2c = open_board_i2c()?;
    let codec_addr = detect_es8311_addr(&mut i2c)?;
    let chip_id1 = es8311_read_reg(&mut i2c, codec_addr, ES8311_CHD1_REGFD)?;
    let chip_id2 = es8311_read_reg(&mut i2c, codec_addr, ES8311_CHD2_REGFE)?;
    let version = es8311_read_reg(&mut i2c, codec_addr, ES8311_CHVER_REGFF)?;
    info!(
        "ES8311 detected @0x{:02X}: chip={:02X}{:02X} version={:02X} sample_rate={}Hz",
        codec_addr, chip_id1, chip_id2, version, sample_rate
    );

    es8311_write_reg(&mut i2c, codec_addr, ES8311_GPIO_REG44, 0x08)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_GPIO_REG44, 0x08)?;

    es8311_write_reg(&mut i2c, codec_addr, ES8311_CLK_MANAGER_REG01, 0x30)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_CLK_MANAGER_REG02, 0x00)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_CLK_MANAGER_REG03, 0x10)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_ADC_REG16, 0x24)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_CLK_MANAGER_REG04, 0x10)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_CLK_MANAGER_REG05, 0x00)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_SYSTEM_REG0B, 0x00)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_SYSTEM_REG0C, 0x00)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_SYSTEM_REG10, 0x1F)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_SYSTEM_REG11, 0x7F)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_RESET_REG00, 0x80)?;

    let mut reset_reg = es8311_read_reg(&mut i2c, codec_addr, ES8311_RESET_REG00)?;
    reset_reg &= 0xBF;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_RESET_REG00, reset_reg)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_CLK_MANAGER_REG01, 0x3F)?;

    configure_es8311_sample_rate(&mut i2c, codec_addr, sample_rate)?;

    es8311_write_reg(&mut i2c, codec_addr, ES8311_SYSTEM_REG13, 0x10)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_ADC_REG1B, 0x0A)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_ADC_REG1C, 0x6A)?;

    let dac_iface = es8311_read_reg(&mut i2c, codec_addr, ES8311_SDPIN_REG09)? & 0xFC;
    let adc_iface = es8311_read_reg(&mut i2c, codec_addr, ES8311_SDPOUT_REG0A)? & 0xFC;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_SDPIN_REG09, dac_iface | 0x0C)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_SDPOUT_REG0A, adc_iface | 0x0C)?;

    let dac_start = es8311_read_reg(&mut i2c, codec_addr, ES8311_SDPIN_REG09)? & !0x40;
    let adc_stop = es8311_read_reg(&mut i2c, codec_addr, ES8311_SDPOUT_REG0A)? | 0x40;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_SDPIN_REG09, dac_start)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_SDPOUT_REG0A, adc_stop)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_ADC_REG17, 0xBF)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_SYSTEM_REG0E, 0x02)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_SYSTEM_REG12, 0x00)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_SYSTEM_REG14, 0x1A)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_SYSTEM_REG0D, 0x01)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_ADC_REG15, 0x40)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_DAC_REG37, 0x08)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_GP_REG45, 0x00)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_GPIO_REG44, 0x58)?;

    let mute_reg = es8311_read_reg(&mut i2c, codec_addr, ES8311_DAC_REG31)? & 0x9F;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_DAC_REG31, mute_reg)?;
    es8311_write_reg(&mut i2c, codec_addr, ES8311_DAC_REG32, 0xC0)?;

    info!("ES8311 output path enabled");

    thread::sleep(Duration::from_millis(60));
    Ok(())
}

fn configure_es8311_sample_rate(i2c: &mut I2cDriver<'_>, codec_addr: u8, sample_rate: u32) -> Result<()> {
    let (reg02, reg03, reg04, reg05, reg06, reg07, reg08) = match sample_rate {
        24_000 => (0x00, 0x10, 0x10, 0x00, 0x03, 0x00, 0xFF),
        16_000 => (0x08, 0x10, 0x10, 0x00, 0x03, 0x00, 0xFF),
        32_000 => (0x18, 0x10, 0x10, 0x00, 0x03, 0x00, 0xFF),
        48_000 => (0x00, 0x10, 0x10, 0x00, 0x03, 0x00, 0xFF),
        other => {
            warn!("ES8311 sample rate {}Hz not in tuned table, using 24kHz clock plan", other);
            (0x00, 0x10, 0x10, 0x00, 0x03, 0x00, 0xFF)
        }
    };

    es8311_write_reg(i2c, codec_addr, ES8311_CLK_MANAGER_REG02, reg02)?;
    es8311_write_reg(i2c, codec_addr, ES8311_CLK_MANAGER_REG03, reg03)?;
    es8311_write_reg(i2c, codec_addr, ES8311_CLK_MANAGER_REG04, reg04)?;
    es8311_write_reg(i2c, codec_addr, ES8311_CLK_MANAGER_REG05, reg05)?;
    es8311_write_reg(i2c, codec_addr, ES8311_CLK_MANAGER_REG06, reg06)?;
    es8311_write_reg(i2c, codec_addr, ES8311_CLK_MANAGER_REG07, reg07)?;
    es8311_write_reg(i2c, codec_addr, ES8311_CLK_MANAGER_REG08, reg08)?;
    Ok(())
}

fn open_board_i2c() -> Result<I2cDriver<'static>> {
    let peripherals = unsafe { Peripherals::steal() };
    let config = I2cConfig::new().baudrate(100.kHz().into());
    let i2c = I2cDriver::new(
        peripherals.i2c1,
        unsafe { AnyIOPin::steal(BOARD_IO_SDA_PIN as u8) },
        unsafe { AnyIOPin::steal(BOARD_IO_SCL_PIN as u8) },
        &config,
    )
    .map_err(|err| anyhow!("board i2c open failed on SDA={} SCL={}: {}", BOARD_IO_SDA_PIN, BOARD_IO_SCL_PIN, err))?;
    Ok(i2c)
}

fn detect_es8311_addr(i2c: &mut I2cDriver<'_>) -> Result<u8> {
    let mut last_error = None;
    for addr in ES8311_ADDRS {
        match es8311_read_reg(i2c, addr, ES8311_CHD1_REGFD) {
            Ok(chip) => {
                info!("ES8311 probe ok at 0x{:02X}, chip id high=0x{:02X}", addr, chip);
                return Ok(addr);
            }
            Err(err) => {
                warn!("ES8311 probe failed at 0x{:02X}: {}", addr, err);
                last_error = Some(err);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("no ES8311 address responded")))
}

fn es8311_write_reg(i2c: &mut I2cDriver<'_>, codec_addr: u8, reg: u8, value: u8) -> Result<()> {
    i2c.write(codec_addr, &[reg, value], 100)
        .map_err(|err| anyhow!("es8311 addr 0x{codec_addr:02X} write 0x{reg:02X} failed: {err}"))?;
    Ok(())
}

fn es8311_read_reg(i2c: &mut I2cDriver<'_>, codec_addr: u8, reg: u8) -> Result<u8> {
    let mut buf = [0u8; 1];
    i2c.write_read(codec_addr, &[reg], &mut buf, 100)
        .map_err(|err| anyhow!("es8311 addr 0x{codec_addr:02X} read 0x{reg:02X} failed: {err}"))?;
    Ok(buf[0])
}

fn configure_board_io_expander_bit(bit: u8, level_high: bool) -> Result<()> {
    let peripherals = unsafe { Peripherals::steal() };
    let config = I2cConfig::new().baudrate(400.kHz().into());
    let mut i2c = I2cDriver::new(
        peripherals.i2c1,
        unsafe { AnyIOPin::steal(BOARD_IO_SDA_PIN as u8) },
        unsafe { AnyIOPin::steal(BOARD_IO_SCL_PIN as u8) },
        &config,
    )
    .map_err(|err| anyhow!("io expander i2c open failed on SDA={} SCL={}: {}", BOARD_IO_SDA_PIN, BOARD_IO_SCL_PIN, err))?;

    let mut last_error = None;
    for addr in BOARD_IO_EXPANDER_ADDRS {
        match configure_expander_bit_at_addr(&mut i2c, addr, bit, level_high) {
            Ok(value) => {
                info!("Board expander @0x{:02X} bit {} set to {} (0x{:02X})", addr, bit, if level_high { 1 } else { 0 }, value);
                return Ok(());
            }
            Err(err) => {
                warn!("Board expander probe at 0x{:02X} failed: {}", addr, err);
                last_error = Some(err);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("no board expander address responded")))
}

fn configure_expander_bit_at_addr(i2c: &mut I2cDriver<'_>, addr: u8, bit: u8, level_high: bool) -> Result<u8> {
    i2c.write(addr, &[0x01, 0x03], 100)
        .map_err(|err| anyhow!("write output reg failed: {}", err))?;
    i2c.write(addr, &[0x03, 0xF8], 100)
        .map_err(|err| anyhow!("write config reg failed: {}", err))?;

    let mut current = [0u8; 1];
    i2c.write_read(addr, &[0x01], &mut current, 100)
        .map_err(|err| anyhow!("read output reg failed: {}", err))?;
    let value = if level_high {
        current[0] | (1 << bit)
    } else {
        current[0] & !(1 << bit)
    };
    i2c.write(addr, &[0x01, value], 100)
        .map_err(|err| anyhow!("set bit {} failed: {}", bit, err))?;
    Ok(value)
}
