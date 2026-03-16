use std::ffi::CString;
use std::io::{self, Read};
use std::os::fd::AsRawFd;

use clap::Parser;
use pangocairo::{cairo, pango};
use wayrs_client::protocol::*;
use wayrs_client::{Connection, EventCtx, IoMode};
use wayrs_protocols::wlr_layer_shell_unstable_v1::*;
use wayrs_utils::keyboard::{Keyboard, KeyboardEvent, KeyboardHandler, xkb};
use wayrs_utils::seats::{SeatHandler, Seats};
use wayrs_utils::shm_alloc::{BufferSpec, ShmAlloc};

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
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

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
        opacity: args.opacity,
        fullscreen: args.fullscreen,
        exit: false,
    };

    while !state.exit {
        conn.flush(IoMode::Blocking)?;
        let mut fds = [libc::pollfd {
            fd: conn.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        unsafe { libc::poll(fds.as_mut_ptr(), 1, -1) };
        match conn.recv_events(IoMode::NonBlocking) {
            Ok(()) => conn.dispatch_events(&mut state),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => (),
            Err(e) => return Err(e.into()),
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
    opacity: f64,
    fullscreen: bool,
    exit: bool,
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

        cairo_ctx.set_source_rgba(0.05, 0.05, 0.05, self.opacity);
        cairo_ctx.fill_preserve().unwrap();

        if !self.fullscreen {
            cairo_ctx.set_source_rgba(0.3, 0.3, 0.3, self.opacity);
            cairo_ctx.set_line_width(border_width);
            cairo_ctx.stroke().unwrap();
        }

        let pango_ctx = pangocairo::functions::create_context(&cairo_ctx);
        let layout = pango::Layout::new(&pango_ctx);
        layout.set_markup(&self.markup);
        let font = pango::FontDescription::from_string("Sans 24");
        layout.set_font_description(Some(&font));

        let (text_width, text_height) = layout.pixel_size();
        cairo_ctx.move_to(
            (box_width as f64 - text_width as f64) / 2.0,
            (box_height as f64 - text_height as f64) / 2.0,
        );
        cairo_ctx.set_source_rgba(1.0, 1.0, 1.0, 1.0);
        pangocairo::functions::show_layout(&cairo_ctx, &layout);

        self.wl_surface
            .damage_buffer(conn, 0, 0, self.width as i32, self.height as i32);
        self.wl_surface
            .attach(conn, Some(buffer.into_wl_buffer()), 0, 0);
        self.wl_surface.commit(conn);
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
        if event.keysym == xkb::Keysym::Escape {
            self.exit = true;
            conn.break_dispatch_loop();
        }
    }
    fn key_released(&mut self, _: &mut Connection<Self>, _: KeyboardEvent) {}
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
