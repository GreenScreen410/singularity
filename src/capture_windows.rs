// Windows desktop capture via Windows.Graphics.Capture (windows-capture crate).
// Runs on its own thread and pushes each BGRA8 frame into the shared buffer.

use crate::Shared;
use windows_capture::{
    capture::{Context, GraphicsCaptureApiHandler},
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    monitor::Monitor,
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    },
};

struct Handler {
    shared: Shared,
    scratch: Vec<u8>,
    got_first: bool,
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = Shared;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            shared: ctx.flags,
            scratch: Vec::new(),
            got_first: false,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let width = frame.width();
        let height = frame.height();
        let buffer = frame.buffer()?;
        // Depad rows into our reusable scratch buffer -> tight width*4 stride.
        let bytes = buffer.as_nopadding_buffer(&mut self.scratch);
        if !self.got_first {
            eprintln!("capture: first frame arrived ({width}x{height})");
            self.got_first = true;
        }

        let mut g = self.shared.lock().unwrap();
        if g.data.len() != bytes.len() {
            g.data.resize(bytes.len(), 0);
        }
        g.data.copy_from_slice(bytes);
        g.width = width;
        g.height = height;
        g.version = g.version.wrapping_add(1);
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        eprintln!("capture: session closed");
        Ok(())
    }
}

fn run(
    cursor: CursorCaptureSettings,
    border: DrawBorderSettings,
    shared: Shared,
) -> Result<(), String> {
    let monitor = Monitor::primary().map_err(|e| format!("no primary monitor: {e:?}"))?;
    let settings = Settings::new(
        monitor,
        cursor,
        border,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        shared,
    );
    // Blocking: runs the capture message loop on this thread until closed.
    Handler::start(settings).map_err(|e| format!("{e:?}"))
}

/// Spawn a background thread that captures the primary monitor forever.
/// Preferred: cursor excluded (the real cursor is OS-drawn on top of the
/// overlay, so a captured copy would ghost near the hole) and no yellow
/// capture-indicator border. Both toggles are unsupported on some older
/// Windows 10 builds, so fall back progressively if starting fails.
pub fn start(shared: Shared) {
    std::thread::spawn(move || {
        let attempts: [(CursorCaptureSettings, DrawBorderSettings, &str); 3] = [
            (
                CursorCaptureSettings::WithoutCursor,
                DrawBorderSettings::WithoutBorder,
                "cursor excluded, no border",
            ),
            (
                CursorCaptureSettings::WithoutCursor,
                DrawBorderSettings::Default,
                "cursor excluded, default border",
            ),
            (
                CursorCaptureSettings::Default,
                DrawBorderSettings::Default,
                "default settings",
            ),
        ];
        for (cursor, border, label) in attempts {
            eprintln!("capture: starting ({label})");
            match run(cursor, border, shared.clone()) {
                Ok(()) => return, // session ran and closed normally
                Err(e) => eprintln!("capture: {label} failed: {e}"),
            }
        }
        eprintln!("capture: all attempts failed");
    });
}
