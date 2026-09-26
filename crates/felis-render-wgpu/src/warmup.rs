//! Renderer setup that needs no window, started before the window exists.

use std::{
    future::Future,
    pin::pin,
    sync::{
        Arc,
        mpsc::{self, Receiver},
    },
    task::{Context, Poll, Wake, Waker},
    thread::{self, JoinHandle, Thread},
    time::Duration,
};

use felis_shaping::{FontStack, ShapingError};
use wgpu::{Adapter, Device, Instance, Queue};

use crate::{FaceSpec, RendererConfig};

/// A driver that stalls must not stall an exit that never needed the
/// GPU (an unreachable daemon), and returning from `main` while the
/// driver is still inside instance or device creation runs its exit
/// handlers under that call: wait this long, then leave the thread.
const UNUSED_GPU_GRACE: Duration = Duration::from_secs(2);

/// The GPU instance, adapter and device, and the font stack, each on
/// its own thread; [`crate::Renderer::new_with_config`] takes them.
pub struct Warmup {
    gpu: Option<Receiver<WarmGpu>>,
    fonts: Option<WarmFonts>,
}

pub(crate) struct WarmGpu {
    pub(crate) instance: Instance,
    /// `None` when either request failed; the window-bound path then
    /// repeats it, so the error the user sees is the one it reports.
    pub(crate) device: Option<(Adapter, Device, Queue)>,
}

struct WarmFonts {
    inputs: FontInputs,
    stack: JoinHandle<Result<FontStack, ShapingError>>,
}

/// Every [`RendererConfig`] field font discovery reads.
#[derive(Clone, PartialEq)]
struct FontInputs {
    files: Vec<std::path::PathBuf>,
    family: Option<String>,
    features: Vec<String>,
    fallbacks: Vec<FaceSpec>,
    bold: FaceSpec,
    italic: FaceSpec,
    bold_italic: FaceSpec,
}

impl FontInputs {
    fn of(cfg: &RendererConfig) -> Self {
        Self {
            files: cfg.font_files.clone(),
            family: cfg.font_family.clone(),
            features: cfg.font_features.clone(),
            fallbacks: cfg.font_fallbacks.clone(),
            bold: cfg.font_bold.clone(),
            italic: cfg.font_italic.clone(),
            bold_italic: cfg.font_bold_italic.clone(),
        }
    }

    fn discover(&self) -> Result<FontStack, ShapingError> {
        crate::discover_fonts(
            &self.files,
            self.family.as_deref(),
            &self.features,
            &self.fallbacks,
            &felis_shaping::StyleFaces {
                bold: &self.bold,
                italic: &self.italic,
                bold_italic: &self.bold_italic,
            },
        )
    }
}

impl Warmup {
    #[must_use]
    pub fn start_gpu() -> Self {
        let (tx, rx) = mpsc::sync_channel(1);
        let spawned = thread::Builder::new()
            .name("felis-gpu-warmup".into())
            .spawn(move || {
                let instance = crate::new_instance();
                let device = block_on(async {
                    let adapter = crate::request_adapter(&instance, None).await.ok()?;
                    let (device, queue) = crate::request_device(&adapter).await.ok()?;
                    Some((adapter, device, queue))
                });
                drop(tx.send(WarmGpu { instance, device }));
            });
        Self {
            gpu: spawned.is_ok().then_some(rx),
            fonts: None,
        }
    }

    /// Separate from [`Self::start_gpu`] so that a launch which never
    /// reaches a window does not log font diagnostics.
    pub fn start_fonts(&mut self, cfg: &RendererConfig) {
        self.fonts = WarmFonts::start(FontInputs::of(cfg));
    }

    pub(crate) fn take_gpu(&mut self) -> Option<WarmGpu> {
        self.gpu.take()?.recv().ok()
    }

    /// `None` when the config's font inputs changed since
    /// [`Self::start_fonts`] (a reload before the window existed); the
    /// caller discovers again.
    pub(crate) fn take_fonts(
        &mut self,
        cfg: &RendererConfig,
    ) -> Option<Result<FontStack, ShapingError>> {
        let fonts = self.fonts.take()?;
        if fonts.inputs != FontInputs::of(cfg) {
            return None;
        }
        fonts.stack.join().ok()
    }
}

impl WarmFonts {
    fn start(inputs: FontInputs) -> Option<Self> {
        let thread_inputs = inputs.clone();
        let stack = thread::Builder::new()
            .name("felis-font-warmup".into())
            .spawn(move || thread_inputs.discover())
            .ok()?;
        Some(Self { inputs, stack })
    }
}

impl Drop for Warmup {
    fn drop(&mut self) {
        if let Some(gpu) = self.gpu.take() {
            drop(gpu.recv_timeout(UNUSED_GPU_GRACE));
        }
    }
}

struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fonts_only(cfg: &RendererConfig) -> Warmup {
        let mut warmup = Warmup {
            gpu: None,
            fonts: None,
        };
        warmup.start_fonts(cfg);
        warmup
    }

    #[test]
    fn warm_font_stack_is_handed_over_when_the_config_is_unchanged() {
        let cfg = RendererConfig::default();
        let mut warmup = fonts_only(&cfg);
        let stack = warmup.take_fonts(&cfg).expect("joined").expect("monospace");
        let fresh = FontInputs::of(&cfg).discover().expect("monospace");
        assert_eq!(stack.len(), fresh.len());
    }

    #[test]
    fn warm_font_stack_is_discarded_when_a_font_input_changed() {
        let started = RendererConfig::default();
        let mut warmup = fonts_only(&started);
        let reloaded = RendererConfig {
            font_features: vec!["-liga".to_owned()],
            ..RendererConfig::default()
        };
        assert!(warmup.take_fonts(&reloaded).is_none());
    }

    #[test]
    fn warm_font_stack_is_discarded_when_the_font_files_changed() {
        let started = RendererConfig::default();
        let mut warmup = fonts_only(&started);
        let pinned = RendererConfig {
            font_files: vec!["/nonexistent/felis-font.ttf".into()],
            ..RendererConfig::default()
        };
        assert!(warmup.take_fonts(&pinned).is_none());
    }

    #[test]
    fn warm_font_stack_ignores_non_font_config() {
        let started = RendererConfig::default();
        let mut warmup = fonts_only(&started);
        let recolored = RendererConfig {
            theme_fg: Some("#ffffff".to_owned()),
            font_size_physical_px: Some(28.0),
            ..RendererConfig::default()
        };
        assert!(warmup.take_fonts(&recolored).is_some());
    }

    #[test]
    fn block_on_drives_a_future_woken_from_another_thread() {
        struct WokenLater(Option<JoinHandle<()>>, Arc<std::sync::atomic::AtomicBool>);
        impl Future for WokenLater {
            type Output = u8;
            fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u8> {
                if self.1.load(std::sync::atomic::Ordering::Acquire) {
                    return Poll::Ready(7);
                }
                if self.0.is_none() {
                    let (done, waker) = (Arc::clone(&self.1), cx.waker().clone());
                    self.0 = Some(thread::spawn(move || {
                        done.store(true, std::sync::atomic::Ordering::Release);
                        waker.wake();
                    }));
                }
                Poll::Pending
            }
        }
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        assert_eq!(block_on(WokenLater(None, flag)), 7);
    }
}
