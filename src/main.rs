use std::ffi::CString;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::PathBuf;

use clap::Parser;
use pangocairo::{cairo, pango};
use wayrs_client::protocol::*;
use wayrs_client::{Connection, EventCtx, IoMode};
use wayrs_protocols::wlr_layer_shell_unstable_v1::*;
use wayrs_utils::keyboard::{Keyboard, KeyboardEvent, KeyboardHandler, xkb};
use wayrs_utils::seats::{SeatHandler, Seats};
use wayrs_utils::shm_alloc::{BufferSpec, ShmAlloc};

/// Milliseconds between auto-scroll steps while a scroll key is held.
const SCROLL_INTERVAL_MS: i32 = 50;

/// Resolved display settings: built-in defaults, overridden by the config file.
#[derive(Debug, Clone, Copy)]
struct Settings {
    min_font: f64,
    padding: f64,
    background: (f64, f64, f64),
    border: (f64, f64, f64),
    text: (f64, f64, f64),
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            min_font: 10.0,
            padding: 24.0,
            background: (0.05, 0.05, 0.05),
            border: (0.3, 0.3, 0.3),
            text: (1.0, 1.0, 1.0),
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

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("osd-window").join("config.toml"))
}

/// Load settings, falling back to defaults for anything missing or invalid.
fn load_settings() -> Settings {
    let mut settings = Settings::default();

    let Some(path) = config_path() else {
        return settings;
    };
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return settings;
    };
    let Ok(table) = toml::from_str::<toml::Table>(&contents) else {
        return settings;
    };

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

    settings
}

const DEFAULT_CONFIG: &str = r##"# osd-window configuration
# All keys are optional; missing values fall back to these defaults.

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
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if args.write_config {
        return write_default_config();
    }

    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let input = input.trim();

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
        markup: input.to_string(),
        font: args.font,
        opacity: args.opacity,
        fullscreen: args.fullscreen,
        exit: false,
        scroll_y: 0.0,
        max_scroll: 0.0,
        line_height: 20.0,
        avail_h: 0.0,
        settings: load_settings(),
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
        match event.keysym {
            xkb::Keysym::Escape | xkb::Keysym::q | xkb::Keysym::Q => {
                self.exit = true;
                conn.break_dispatch_loop();
            }
            xkb::Keysym::j | xkb::Keysym::Down => {
                self.scroll_dir = 1;
                self.scroll_tick(conn);
            }
            xkb::Keysym::k | xkb::Keysym::Up => {
                self.scroll_dir = -1;
                self.scroll_tick(conn);
            }
            xkb::Keysym::Page_Down => {
                self.scroll_y = (self.scroll_y + self.avail_h).min(self.max_scroll);
                self.draw(conn);
            }
            xkb::Keysym::Page_Up => {
                self.scroll_y = (self.scroll_y - self.avail_h).max(0.0);
                self.draw(conn);
            }
            _ => {}
        }
    }
    fn key_released(&mut self, _: &mut Connection<Self>, event: KeyboardEvent) {
        match event.keysym {
            xkb::Keysym::j | xkb::Keysym::Down | xkb::Keysym::k | xkb::Keysym::Up => {
                self.scroll_dir = 0;
            }
            _ => {}
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
