// macOS desktop capture via ScreenCaptureKit (objc2 bindings).
//
// STATUS: structurally complete but UNTESTED on real hardware - no Mac in the
// dev loop. It is kept honest with `cargo check --target aarch64-apple-darwin`
// (the objc2 crates are pure Rust declarations, so they type-check anywhere).
// If you run this on a Mac and something misbehaves, please open an issue.
//
// Requires the Screen Recording permission (System Settings > Privacy &
// Security > Screen Recording). The first launch triggers the system prompt;
// grant it and relaunch.
//
// Self-capture feedback is prevented in main.rs by setting the overlay
// NSWindow's sharingType to NSWindowSharingNone - the macOS equivalent of
// Windows' WDA_EXCLUDEFROMCAPTURE.

use crate::Shared;
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass, Message};
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput,
    SCStreamOutputType,
};

// kCVPixelFormatType_32BGRA - matches the Bgra8UnormSrgb texture upload path.
const FORMAT_32BGRA: u32 = u32::from_be_bytes(*b"BGRA");

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "SingularityStreamOutput"]
    #[ivars = Shared]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_did_output(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            let Some(image) = (unsafe { sample_buffer.image_buffer() }) else {
                return;
            };
            // CVPixelBufferRef and CVImageBufferRef are the same object in
            // CoreVideo's C API; the bindings just expose them as two types.
            let pb: &CVPixelBuffer =
                unsafe { &*((&*image) as *const _ as *const CVPixelBuffer) };

            unsafe {
                if CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly) != 0 {
                    return;
                }
                let w = CVPixelBufferGetWidth(pb);
                let h = CVPixelBufferGetHeight(pb);
                let stride = CVPixelBufferGetBytesPerRow(pb);
                let base = CVPixelBufferGetBaseAddress(pb) as *const u8;
                if !base.is_null() && w > 0 && h > 0 {
                    let mut g = self.ivars().lock().unwrap();
                    let tight = w * 4;
                    if g.data.len() != tight * h {
                        g.data.resize(tight * h, 0);
                    }
                    // depad rows -> tight width*4 stride, like the Windows path
                    for y in 0..h {
                        let src = std::slice::from_raw_parts(base.add(y * stride), tight);
                        g.data[y * tight..(y + 1) * tight].copy_from_slice(src);
                    }
                    g.width = w as u32;
                    g.height = h as u32;
                    g.version = g.version.wrapping_add(1);
                }
                CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly);
            }
        }
    }
);

impl Output {
    fn new(shared: Shared) -> Retained<Self> {
        let this = Self::alloc().set_ivars(shared);
        unsafe { msg_send![super(this), init] }
    }
}

/// Start capturing the primary display. ScreenCaptureKit delivers frames on
/// its own dispatch queue, so this returns immediately.
pub fn start(shared: Shared) {
    eprintln!("capture: requesting shareable content (screen-recording permission)");
    let handler = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        if content.is_null() {
            let msg = unsafe { error.as_ref().map(|e| e.localizedDescription()) };
            eprintln!(
                "capture: no shareable content - grant Screen Recording permission \
                 in System Settings and relaunch ({msg:?})"
            );
            return;
        }
        let content = unsafe { &*content };
        let displays = unsafe { content.displays() };
        // display selection follows the startup monitor choice; live monitor
        // switching is not wired up on macOS yet
        let idx = shared.lock().unwrap().monitor_index;
        let display = if idx < displays.count() {
            Some(unsafe { displays.objectAtIndex_unchecked(idx) }.retain())
        } else {
            displays.firstObject()
        };
        let Some(display) = display else {
            eprintln!("capture: no displays available");
            return;
        };

        unsafe {
            // Whole display; our own window is excluded via sharingType, so
            // no per-window exclusion list is needed here.
            let filter = SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::new(),
            );

            let config = SCStreamConfiguration::new();
            // SCDisplay reports points; on Retina the stream still delivers
            // a full-resolution buffer scaled to this size. Good enough for
            // a lensed background - revisit with pointPixelScale if blurry.
            config.setWidth(display.width() as usize);
            config.setHeight(display.height() as usize);
            config.setPixelFormat(FORMAT_32BGRA);
            // like the Windows path: the real cursor is drawn above the
            // overlay anyway, a captured copy would only ghost near the hole
            config.setShowsCursor(false);

            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                None,
            );
            let output = Output::new(shared.clone());
            if let Err(e) = stream.addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*output),
                SCStreamOutputType::Screen,
                None, // SCK picks a queue
            ) {
                eprintln!("capture: addStreamOutput failed: {e:?}");
                return;
            }

            let started = RcBlock::new(|err: *mut NSError| {
                if err.is_null() {
                    eprintln!("capture: started");
                } else {
                    let msg = err.as_ref().map(|e| e.localizedDescription());
                    eprintln!("capture: start failed: {msg:?}");
                }
            });
            stream.startCaptureWithCompletionHandler(Some(&started));

            // The stream and output must outlive this callback; the overlay
            // captures for the app's whole lifetime, so leak them.
            std::mem::forget(stream);
            std::mem::forget(output);
        }
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
}
