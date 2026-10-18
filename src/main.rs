use std::collections::HashMap;
use std::ffi::CString;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Parser;
use xkeysym::Keysym;
use pangocairo::{cairo, pango};
use wayrs_client::protocol::*;
use wayrs_client::{Connection, EventCtx, IoMode};
use wayrs_protocols::wlr_layer_shell_unstable_v1::*;
use wayrs_utils::keyboard::{Keyboard, KeyboardEvent, KeyboardHandler, xkb};
use wayrs_utils::seats::{SeatHandler, Seats};
use wayrs_utils::shm_alloc::{BufferSpec, ShmAlloc};

/// Milliseconds between auto-scroll steps while a scroll key is held.
const SCROLL_INTERVAL_MS: i32 = 50;

#[derive(Debug, Clone)]
struct KeyBindings {
    dismiss: Vec<Keysym>,
    scroll_up: Vec<Keysym>,
    scroll_down: Vec<Keysym>,
    page_up: Vec<Keysym>,
    page_down: Vec<Keysym>,
}

impl Default for KeyBindings {
    fn default() -> Self {
        Self {
            dismiss: vec![Keysym::Escape, Keysym::q, Keysym::Return],
            scroll_up: vec![Keysym::k, Keysym::Up],
            scroll_down: vec![Keysym::j, Keysym::Down],
            page_up: vec![Keysym::Page_Up],
            page_down: vec![Keysym::Page_Down],
        }
    }
}

/// Resolved display settings: built-in defaults, overridden by the config file.
#[derive(Debug, Clone)]
struct Settings {
    min_font: f64,
    padding: f64,
    background: (f64, f64, f64),
    border: (f64, f64, f64),
    text: (f64, f64, f64),
    keys: KeyBindings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            min_font: 10.0,
            padding: 24.0,
            background: (0.05, 0.05, 0.05),
            border: (0.3, 0.3, 0.3),
            text: (1.0, 1.0, 1.0),
            keys: KeyBindings::default(),
        }
    }
}

/// Parse a `#RRGGBB` colour (leading `#` optional) into 0..1 RGB floats.
fn parse_color(s: &str) -> Option<(f64, f64, f64)> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some((r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0))
}

/// Accept a TOML number whether it's written as an integer or a float.
fn as_number(v: &toml::Value) -> Option<f64> {
    v.as_float().or_else(|| v.as_integer().map(|i| i as f64))
}

/// Map common friendly names to canonical keysym names.
fn normalize_key_name(name: &str) -> String {
    let trimmed = name.trim();
    match trimmed.to_lowercase().as_str() {
        "esc" | "escape" => "Escape".to_string(),
        "enter" | "return" => "Return".to_string(),
        "space" | "spc" => "space".to_string(),
        "up" => "Up".to_string(),
        "down" => "Down".to_string(),
        "left" => "Left".to_string(),
        "right" => "Right".to_string(),
        "pgup" | "pageup" | "page_up" | "prior" => "Page_Up".to_string(),
        "pgdn" | "pagedown" | "page_down" | "next" => "Page_Down".to_string(),
        "del" | "delete" => "Delete".to_string(),
        "backspace" | "bs" => "BackSpace".to_string(),
        "tab" => "Tab".to_string(),
        "home" => "Home".to_string(),
        "end" => "End".to_string(),
        _ => trimmed.to_string(),
    }
}

/// Resolve a key name from config to a `Keysym`, or `None` if unknown.
///
/// Tries an exact match first, then falls back to a case-insensitive lookup so
/// friendly names like `home` resolve to `Home`.
fn resolve_keysym(name: &str) -> Option<Keysym> {
    let canonical = normalize_key_name(name);
    let mut keysym = xkb::keysym_from_name(&canonical, xkb::KEYSYM_NO_FLAGS);
    if keysym == Keysym::NoSymbol {
        keysym = xkb::keysym_from_name(&canonical, xkb::KEYSYM_CASE_INSENSITIVE);
    }
    (keysym != Keysym::NoSymbol).then_some(keysym)
}

/// Human-friendly keysym name for `--get-key` output and error messages.
fn keysym_display_name(keysym: Keysym) -> String {
    match xkb::keysym_get_name(keysym).as_str() {
        "" => format!("0x{:x}", keysym.raw()),
        "Prior" => "Page_Up".to_string(),
        "Next" => "Page_Down".to_string(),
        other => other.to_string(),
    }
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("osd-window").join("config.toml"))
}



/// Hard error on malformed config (instead of silent fallback).
fn load_settings() -> anyhow::Result<Settings> {
    let mut settings = Settings::default();

    let Some(path) = config_path() else {
        return Ok(settings);
    };
    if !path.exists() {
        return Ok(settings);
    }

    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read config {}", path.display()))?;

    let table: toml::Table = toml::from_str(&contents)
        .with_context(|| format!("invalid TOML in {}", path.display()))?;

    if let Some(v) = table.get("min_font").and_then(as_number) {
        settings.min_font = v;
    }
    if let Some(v) = table.get("padding").and_then(as_number) {
        settings.padding = v;
    }
    if let Some(c) = table.get("background").and_then(|v| v.as_str()).and_then(parse_color) {
        settings.background = c;
    }
    if let Some(c) = table.get("border").and_then(|v| v.as_str()).and_then(parse_color) {
        settings.border = c;
    }
    if let Some(c) = table.get("text").and_then(|v| v.as_str()).and_then(parse_color) {
        settings.text = c;
    }

    if let Some(keys_table) = table.get("keys").and_then(|v| v.as_table()) {
        settings.keys = parse_key_bindings(&path, keys_table)?;
    }

    Ok(settings)
}

/// Resolve one action's key list, falling back to `default` when not present.
fn load_key_list(
    path: &Path,
    keys_table: &toml::Table,
    action: &str,
    default: &[Keysym],
) -> anyhow::Result<Vec<Keysym>> {
    let Some(value) = keys_table.get(action) else {
        return Ok(default.to_vec());
    };

    let arr = value.as_array().ok_or_else(|| {
        anyhow::anyhow!(
            "[keys].{} in {} must be an array of key names",
            action,
            path.display()
        )
    })?;

    let mut resolved = Vec::with_capacity(arr.len());
    for item in arr {
        let name = item.as_str().ok_or_else(|| {
            anyhow::anyhow!(
                "[keys].{} in {} must contain only strings",
                action,
                path.display()
            )
        })?;
        let keysym = resolve_keysym(name).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown key name '{}' in [keys].{} of {} \
                 (run `osd-window --get-key` to discover valid names)",
                name,
                action,
                path.display()
            )
        })?;
        resolved.push(keysym);
    }
    Ok(resolved)
}

/// Parse the `[keys]` table and reject duplicate key assignments.
fn parse_key_bindings(path: &Path, keys_table: &toml::Table) -> anyhow::Result<KeyBindings> {
    let defaults = KeyBindings::default();
    let keys = KeyBindings {
        dismiss: load_key_list(path, keys_table, "dismiss", &defaults.dismiss)?,
        scroll_up: load_key_list(path, keys_table, "scroll_up", &defaults.scroll_up)?,
        scroll_down: load_key_list(path, keys_table, "scroll_down", &defaults.scroll_down)?,
        page_up: load_key_list(path, keys_table, "page_up", &defaults.page_up)?,
        page_down: load_key_list(path, keys_table, "page_down", &defaults.page_down)?,
    };

    validate_key_bindings(&keys)?;
    Ok(keys)
}

/// Reject any keysym bound to more than one action.
fn validate_key_bindings(keys: &KeyBindings) -> anyhow::Result<()> {
    let mut seen: HashMap<Keysym, &'static str> = HashMap::new();
    for (action, list) in [
        ("dismiss", keys.dismiss.as_slice()),
        ("scroll_up", keys.scroll_up.as_slice()),
        ("scroll_down", keys.scroll_down.as_slice()),
        ("page_up", keys.page_up.as_slice()),
        ("page_down", keys.page_down.as_slice()),
    ] {
        for &keysym in list {
            if let Some(previous) = seen.insert(keysym, action) {
                anyhow::bail!(
                    "key '{}' is assigned to multiple actions ({} and {})",
                    keysym_display_name(keysym),
                    previous,
                    action
                );
            }
        }
    }
    Ok(())
}

const DEFAULT_CONFIG: &str = r##"# osd-window configuration
# Every value is optional; anything you omit falls back to the built-in default.

# Minimum font size (points) before auto-fit falls back to scrolling.
min_font = 10

# Space between the box edge and the text, in pixels.
padding = 24

# Box background colour (#RRGGBB).
background = "#0d0d0d"

# Box border colour (#RRGGBB).
border = "#4d4d4d"

# Base text colour (#RRGGBB). Markup spans can override this.
text = "#ffffff"

# Keybindings. Each action takes a list of one or more key names.
#
# Key names are XKB keysym names; run `osd-window --get-key`, press a key, and
# it prints the exact name to use here. A few friendly aliases also work:
#   esc/escape, enter/return, space, up/down/left/right,
#   pgup/pgdn/page_up/page_down, del/delete, backspace, tab, home, end.
# Names are case-insensitive.
#
# A key may only be bound to one action: using the same key in two actions is
# an error. If you override one action, the rest keep their defaults below.
[keys]
dismiss     = ["Escape", "q", "Return"]
scroll_up   = ["k", "Up"]
scroll_down = ["j", "Down"]
page_up     = ["Page_Up"]
page_down   = ["Page_Down"]
"##;

fn write_default_config() -> anyhow::Result<()> {
    let path = config_path()
        .ok_or_else(|| anyhow::anyhow!("could not determine config path (is HOME set?)"))?;

    if path.exists() {
        anyhow::bail!("config file already exists: {}", path.display());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, DEFAULT_CONFIG)?;

    println!("Wrote default config to {}", path.display());
    Ok(())
}

#[derive(Parser)]
struct Args {
    /// Opacity of the window background (0.0 to 1.0)
    #[arg(short, long, default_value_t = 0.95)]
    opacity: f64,

    /// Width of the visible box
    #[arg(short, long, default_value_t = 600)]
    width: u32,

    /// Height of the visible box
    #[arg(short = 'H', long, default_value_t = 150)]
    height: u32,

    /// Show the box fullscreen
    #[arg(short, long)]
    fullscreen: bool,

    /// Font to use for the OSD text
    #[arg(short = 'F', long, default_value = "Sans 24")]
    font: String,

    /// Write a default config file to the config path and exit
    #[arg(long)]
    write_config: bool,

    /// Wait for a keypress and print the key name to use in config, then exit
    #[arg(long)]
    get_key: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if args.write_config {
        return write_default_config();
    }

    // In --get-key mode we show a hint instead of reading stdin for OSD text.
    let markup = if args.get_key {
        "Press any key…".to_string()
    } else {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        input.trim().to_string()
    };

    let mut conn = Connection::connect()?;
    conn.blocking_roundtrip()?;

    let wl_compositor: WlCompositor = conn.bind_singleton(4..=6)?;
    let wlr_layer_shell: ZwlrLayerShellV1 = conn.bind_singleton(2)?;
    let shm_alloc = ShmAlloc::bind(&mut conn)?;
    let seats = Seats::new(&mut conn);

    let wl_surface = wl_compositor.create_surface(&mut conn);

    let layer_surface = wlr_layer_shell.get_layer_surface_with_cb(
        &mut conn,
        wl_surface,
        None,
        zwlr_layer_shell_v1::Layer::Overlay,
        CString::new("osd-window").unwrap(),
        layer_surface_cb,
    );

    // 4. Configure: Exclusive interactivity allows us to catch the Escape key
    layer_surface.set_anchor(
        &mut conn,
        zwlr_layer_surface_v1::Anchor::Top
            | zwlr_layer_surface_v1::Anchor::Bottom
            | zwlr_layer_surface_v1::Anchor::Left
            | zwlr_layer_surface_v1::Anchor::Right,
    );
    layer_surface.set_size(&mut conn, 0, 0);
    layer_surface.set_keyboard_interactivity(
        &mut conn,
        zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive,
    );

    // If fullscreen, set exclusive_zone to -1 to ignore reserved space (like Waybar)
    if args.fullscreen {
        layer_surface.set_exclusive_zone(&mut conn, -1);
    }

    wl_surface.commit(&mut conn);

    let settings = load_settings()?;

    let mut state = State {
        shm_alloc,
        seats,
        keyboards: Vec::new(),
        wl_surface,
        layer_surface,
        width: 0,  // Will be set by Configure event (actual screen size)
        height: 0, // Will be set by Configure event
        target_width: args.width,
        target_height: args.height,
        markup,
        font: args.font,
        opacity: args.opacity,
        fullscreen: args.fullscreen,
        exit: false,
        scroll_y: 0.0,
        max_scroll: 0.0,
        line_height: 20.0,
        avail_h: 0.0,
        settings,
        get_key_mode: args.get_key,
        scroll_dir: 0,
    };

    while !state.exit {
        conn.flush(IoMode::Blocking)?;
        let mut fds = [libc::pollfd {
            fd: conn.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        let timeout = if state.scroll_dir != 0 { SCROLL_INTERVAL_MS } else { -1 };
        let ret = unsafe { libc::poll(fds.as_mut_ptr(), 1, timeout) };
        if ret > 0 {
            match conn.recv_events(IoMode::NonBlocking) {
                Ok(()) => conn.dispatch_events(&mut state),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
                Err(e) => return Err(e.into()),
            }
        } else if ret == 0 {
            state.scroll_tick(&mut conn);
        }
    }

    Ok(())
}

struct State {
    shm_alloc: ShmAlloc,
    seats: Seats,
    keyboards: Vec<Keyboard>,
    wl_surface: WlSurface,
    layer_surface: ZwlrLayerSurfaceV1,
    width: u32,         // Screen width
    height: u32,        // Screen height
    target_width: u32,  // Box width
    target_height: u32, // Box height
    markup: String,
    font: String,
    opacity: f64,
    fullscreen: bool,
    exit: bool,
    scroll_y: f64,
    max_scroll: f64,
    line_height: f64,
    avail_h: f64,
    settings: Settings,
    get_key_mode: bool,
    scroll_dir: i32,
}

impl State {
    fn draw(&mut self, conn: &mut Connection<Self>) {
        if self.width == 0 || self.height == 0 {
            return;
        }

        let (box_width, box_height) = if self.fullscreen {
            (self.width, self.height)
        } else {
            (self.target_width, self.target_height)
        };

        let (buffer, canvas) = self
            .shm_alloc
            .alloc_buffer(
                conn,
                BufferSpec {
                    width: self.width,
                    height: self.height,
                    stride: self.width * 4,
                    format: wl_shm::Format::Argb8888,
                },
            )
            .expect("could not allocate shm buffer");

        let cairo_surf = unsafe {
            cairo::ImageSurface::create_for_data_unsafe(
                canvas.as_mut_ptr(),
                cairo::Format::ARgb32,
                self.width as i32,
                self.height as i32,
                (self.width * 4) as i32,
            )
            .expect("cairo surface")
        };

        let cairo_ctx = cairo::Context::new(&cairo_surf).expect("cairo context");

        // Clear background to transparent
        cairo_ctx.set_operator(cairo::Operator::Source);
        cairo_ctx.set_source_rgba(0.0, 0.0, 0.0, 0.0);
        cairo_ctx.paint().unwrap();
        cairo_ctx.set_operator(cairo::Operator::Over);

        // Center the box
        let x_offset = (self.width as f64 - box_width as f64) / 2.0;
        let y_offset = (self.height as f64 - box_height as f64) / 2.0;
        cairo_ctx.translate(x_offset, y_offset);

        let radius = if self.fullscreen { 0.0 } else { 15.0 };
        let border_width = if self.fullscreen { 0.0 } else { 3.0 };

        cairo_ctx.new_sub_path();
        cairo_ctx.arc(
            radius,
            radius,
            radius,
            180.0 * (std::f64::consts::PI / 180.0),
            270.0 * (std::f64::consts::PI / 180.0),
        );
        cairo_ctx.arc(
            box_width as f64 - radius,
            radius,
            radius,
            270.0 * (std::f64::consts::PI / 180.0),
            360.0 * (std::f64::consts::PI / 180.0),
        );
        cairo_ctx.arc(
            box_width as f64 - radius,
            box_height as f64 - radius,
            radius,
            0.0,
            90.0 * (std::f64::consts::PI / 180.0),
        );
        cairo_ctx.arc(
            radius,
            box_height as f64 - radius,
            radius,
            90.0 * (std::f64::consts::PI / 180.0),
            180.0 * (std::f64::consts::PI / 180.0),
        );
        cairo_ctx.close_path();

        let (r, g, b) = self.settings.background;
        cairo_ctx.set_source_rgba(r, g, b, self.opacity);
        cairo_ctx.fill_preserve().unwrap();

        if !self.fullscreen {
            let (r, g, b) = self.settings.border;
            cairo_ctx.set_source_rgba(r, g, b, self.opacity);
            cairo_ctx.set_line_width(border_width);
            cairo_ctx.stroke().unwrap();
        }

        let pango_ctx = pangocairo::functions::create_context(&cairo_ctx);
        let layout = pango::Layout::new(&pango_ctx);
        layout.set_markup(&self.markup);

        // Wrap long lines so nothing overflows horizontally.
        let avail_w = (box_width as f64 - 2.0 * self.settings.padding).max(50.0);
        let avail_h = (box_height as f64 - 2.0 * self.settings.padding).max(50.0);
        layout.set_width((avail_w * pango::SCALE as f64) as i32);

        // Shrink the font from the configured size down to a floor until it fits.
        let mut font = pango::FontDescription::from_string(&self.font);
        let initial_size = font.size();
        let floor = (self.settings.min_font * pango::SCALE as f64) as i32;
        let mut size = initial_size;
        while size > floor {
            font.set_size(size);
            layout.set_font_description(Some(&font));
            let (w, h) = layout.pixel_size();
            if (w as f64) <= avail_w && (h as f64) <= avail_h {
                break;
            }
            size -= pango::SCALE;
        }
        font.set_size(size);
        layout.set_font_description(Some(&font));

        let (text_width, text_height) = layout.pixel_size();

        // Vertical placement: centered when it fits, top-aligned + scrollable otherwise.
        let overflow = (text_height as f64) > avail_h;
        let base_y = if overflow {
            self.settings.padding
        } else {
            (box_height as f64 - text_height as f64) / 2.0
        };
        self.max_scroll = if overflow {
            (text_height as f64 - avail_h).max(0.0)
        } else {
            0.0
        };
        self.line_height = if layout.line_count() > 0 {
            text_height as f64 / layout.line_count() as f64
        } else {
            20.0
        };
        self.avail_h = avail_h;
        self.scroll_y = self.scroll_y.clamp(0.0, self.max_scroll);

        let text_x = (box_width as f64 - text_width as f64) / 2.0;
        let text_y = base_y - self.scroll_y;

        // Clip the text to the box so scrolled content doesn't bleed outside.
        cairo_ctx.save().unwrap();
        cairo_ctx.new_path();
        cairo_ctx.rectangle(0.0, 0.0, box_width as f64, box_height as f64);
        cairo_ctx.clip();
        cairo_ctx.move_to(text_x, text_y);
        let (r, g, b) = self.settings.text;
        cairo_ctx.set_source_rgba(r, g, b, 1.0);
        pangocairo::functions::show_layout(&cairo_ctx, &layout);
        cairo_ctx.restore().unwrap();

        self.wl_surface
            .damage_buffer(conn, 0, 0, self.width as i32, self.height as i32);
        self.wl_surface
            .attach(conn, Some(buffer.into_wl_buffer()), 0, 0);
        self.wl_surface.commit(conn);
    }

    fn scroll_tick(&mut self, conn: &mut Connection<Self>) {
        if self.scroll_dir == 0 {
            return;
        }
        let step = self.line_height;
        if self.scroll_dir > 0 {
            self.scroll_y = (self.scroll_y + step).min(self.max_scroll);
        } else {
            self.scroll_y = (self.scroll_y - step).max(0.0);
        }
        self.draw(conn);
    }
}

impl SeatHandler for State {
    fn get_seats(&mut self) -> &mut Seats {
        &mut self.seats
    }
    fn keyboard_added(&mut self, conn: &mut Connection<Self>, seat: WlSeat) {
        self.keyboards.push(Keyboard::new(conn, seat));
    }
    fn keyboard_removed(&mut self, _: &mut Connection<Self>, seat: WlSeat) {
        self.keyboards.retain(|k| k.seat() != seat);
    }
    fn seat_added(&mut self, conn: &mut Connection<Self>, seat: WlSeat) {
        seat.get_pointer_with_cb(conn, pointer_cb);
    }
}

impl KeyboardHandler for State {
    fn get_keyboard(&mut self, wl_keyboard: WlKeyboard) -> &mut Keyboard {
        self.keyboards
            .iter_mut()
            .find(|k| k.wl_keyboard() == wl_keyboard)
            .unwrap()
    }
    fn key_presed(&mut self, conn: &mut Connection<Self>, event: KeyboardEvent) {
        if self.get_key_mode {
            let name = keysym_display_name(event.keysym);
            println!("{}", if name.is_empty() { "(no keysym)" } else { &name });
            self.exit = true;
            conn.break_dispatch_loop();
            return;
        }

        let keysym = event.keysym;
        let keys = &self.settings.keys;
        if keys.dismiss.contains(&keysym) {
            self.exit = true;
            conn.break_dispatch_loop();
        } else if keys.scroll_down.contains(&keysym) {
            self.scroll_dir = 1;
            self.scroll_tick(conn);
        } else if keys.scroll_up.contains(&keysym) {
            self.scroll_dir = -1;
            self.scroll_tick(conn);
        } else if keys.page_down.contains(&keysym) {
            self.scroll_y = (self.scroll_y + self.avail_h).min(self.max_scroll);
            self.draw(conn);
        } else if keys.page_up.contains(&keysym) {
            self.scroll_y = (self.scroll_y - self.avail_h).max(0.0);
            self.draw(conn);
        }
    }
    fn key_released(&mut self, _: &mut Connection<Self>, event: KeyboardEvent) {
        if self.get_key_mode {
            return;
        }
        let keysym = event.keysym;
        let keys = &self.settings.keys;
        if keys.scroll_down.contains(&keysym) || keys.scroll_up.contains(&keysym) {
            self.scroll_dir = 0;
        }
    }
}

fn pointer_cb(ctx: EventCtx<State, WlPointer>) {
    if let wl_pointer::Event::Button { .. } = ctx.event {
        ctx.state.exit = true;
        ctx.conn.break_dispatch_loop();
    }
}

fn layer_surface_cb(ctx: EventCtx<State, ZwlrLayerSurfaceV1>) {
    match ctx.event {
        zwlr_layer_surface_v1::Event::Configure(args) => {
            ctx.state.width = args.width;
            ctx.state.height = args.height;
            ctx.proxy.ack_configure(ctx.conn, args.serial);
            ctx.state.draw(ctx.conn);
        }
        zwlr_layer_surface_v1::Event::Closed => {
            ctx.state.exit = true;
        }
        _ => (),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_aliases_and_names() {
        assert_eq!(resolve_keysym("esc"), Some(Keysym::Escape));
        assert_eq!(resolve_keysym("ESCAPE"), Some(Keysym::Escape));
        assert_eq!(resolve_keysym("enter"), Some(Keysym::Return));
        assert_eq!(resolve_keysym("return"), Some(Keysym::Return));
        assert_eq!(resolve_keysym("q"), Some(Keysym::q));
        assert_eq!(resolve_keysym("j"), Some(Keysym::j));
        assert_eq!(resolve_keysym("space"), Some(Keysym::space));
        assert_eq!(resolve_keysym("pgup"), Some(Keysym::Page_Up));
        assert_eq!(resolve_keysym("page_down"), Some(Keysym::Page_Down));
        assert_eq!(resolve_keysym("home"), Some(Keysym::Home));
        assert_eq!(resolve_keysym("BackSpace"), Some(Keysym::BackSpace));
        // Unknown names resolve to None.
        assert_eq!(resolve_keysym("escaoe"), None);
        assert_eq!(resolve_keysym("bogus"), None);
    }

    #[test]
    fn display_name_normalizes_legacy_names() {
        assert_eq!(keysym_display_name(Keysym::Page_Up), "Page_Up");
        assert_eq!(keysym_display_name(Keysym::Page_Down), "Page_Down");
        assert_eq!(keysym_display_name(Keysym::Escape), "Escape");
        assert_eq!(keysym_display_name(Keysym::q), "q");
    }

    #[test]
    fn default_bindings_have_no_duplicates() {
        assert!(validate_key_bindings(&KeyBindings::default()).is_ok());
    }

    fn parse(keys_toml: &str) -> anyhow::Result<KeyBindings> {
        let table: toml::Table = toml::from_str(keys_toml).unwrap();
        parse_key_bindings(Path::new("test.toml"), &table)
    }

    #[test]
    fn parse_valid_bindings() {
        let keys = parse(r#"dismiss = ["esc", "q", "Return"]"#).unwrap();
        assert_eq!(keys.dismiss, vec![Keysym::Escape, Keysym::q, Keysym::Return]);
        // Unspecified actions keep defaults.
        assert_eq!(keys.scroll_down, KeyBindings::default().scroll_down);
    }

    #[test]
    fn parse_rejects_unknown_key() {
        let err = parse(r#"dismiss = ["escaoe"]"#).unwrap_err();
        assert!(err.to_string().contains("unknown key name 'escaoe'"));
        assert!(err.to_string().contains("--get-key"));
    }

    #[test]
    fn parse_rejects_duplicate_key() {
        let err = parse(r#"dismiss = ["j"]"#).unwrap_err();
        assert!(err.to_string().contains("multiple actions"));
        assert!(err.to_string().contains("dismiss"));
        assert!(err.to_string().contains("scroll_down"));
    }

    #[test]
    fn parse_rejects_non_string() {
        let err = parse(r#"dismiss = [1, 2]"#).unwrap_err();
        assert!(err.to_string().contains("only strings"));
    }

    #[test]
    fn parse_rejects_non_array() {
        let err = parse(r#"dismiss = "q""#).unwrap_err();
        assert!(err.to_string().contains("array"));
    }
}
