use crate::app::MultibotView;
use gpui_kit::{Context, Task, Window};
use multibot_core::voice::{DictationPhase, VoiceSettings, append_transcript};

#[derive(Default)]
pub(crate) struct VoiceState {
    pub phase: DictationPhase,
    capture: Option<Capture>,
    task: Option<Task<()>>,
    pub submit_after_finish: bool,
    pub started_at: Option<std::time::Instant>,
    timer: Option<Task<()>>,
    pub error: Option<String>,
}

impl VoiceState {
    pub fn elapsed_label(&self) -> String {
        let seconds = self
            .started_at
            .map(|start| start.elapsed().as_secs())
            .unwrap_or(0);
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }

    pub fn cancel(&mut self) {
        self.timer = None;
        self.started_at = None;
        self.error = None;
        #[cfg(target_os = "windows")]
        if let Some(capture) = self.capture.as_mut() {
            capture.cancel();
        }
        self.capture = None;
        self.submit_after_finish = false;
        self.task = None;
        self.phase.finish();
    }
}

impl Drop for VoiceState {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl MultibotView {
    pub(crate) fn append_dictation_transcript(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if text.is_empty() {
            return;
        }
        let next = append_transcript(self.inputs.prompt_input.read(cx).value().as_str(), text);
        self.inputs.prompt_input.update(cx, |input, cx| {
            input.select_all(window, cx);
            input.replace(next, window, cx);
        });
    }

    pub(crate) fn begin_listening(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.voice.phase.start();
        self.voice.started_at = Some(std::time::Instant::now());
        self.voice.error = None;
        self.voice.timer = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                if !this
                    .update_in(cx, |view, _, cx| {
                        if view.voice.phase != DictationPhase::Listening {
                            return false;
                        }
                        cx.notify();
                        true
                    })
                    .unwrap_or(false)
                {
                    break;
                }
            }
        }));
    }

    pub(crate) fn toggle_dictation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.voice.phase {
            DictationPhase::Listening => {
                self.voice.phase.stop();
                self.voice.timer = None;
                self.voice.capture = None;
                cx.notify();
                return;
            }
            DictationPhase::Finishing => return,
            DictationPhase::Off => {}
        }
        if self.main_surface != crate::state::MainSurface::Chat {
            return;
        }
        let (config, _) = self.settings.voice.resolved(
            cx.global::<crate::state::SharedAppStore>()
                .account
                .deepgram_key
                .as_deref(),
        );
        if let Err(err) = config.validate() {
            self.voice.error = Some(err.to_string());
            self.notice = Some(err.to_string());
            cx.notify();
            return;
        }
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        match Capture::start(config, events_tx) {
            Ok(capture) => self.voice.capture = Some(capture),
            Err(err) => {
                self.voice.error = Some(err.to_string());
                self.notice = Some(err.to_string());
                cx.notify();
                return;
            }
        }
        self.begin_listening(window, cx);
        self.notice = None;
        self.voice.task = Some(cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = events_rx.recv().await {
                let done = matches!(event, VoiceEvent::Finished | VoiceEvent::Error(_));
                if this
                    .update_in(cx, |view, window, cx| {
                        match event {
                            VoiceEvent::Transcript(text) => {
                                view.append_dictation_transcript(&text, window, cx);
                            }
                            VoiceEvent::Error(error) => {
                                view.voice.capture = None;
                                view.voice.phase.finish();
                                view.voice.timer = None;
                                view.voice.started_at = None;
                                view.voice.submit_after_finish = false;
                                view.voice.error = Some(error.clone());
                                view.notice = Some(error);
                            }
                            VoiceEvent::Finished => {
                                view.voice.capture = None;
                                view.voice.phase.finish();
                                view.voice.timer = None;
                                view.voice.started_at = None;
                                if std::mem::take(&mut view.voice.submit_after_finish) {
                                    view.send_current_prompt(window, cx);
                                }
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                    || done
                {
                    break;
                }
            }
        }));
        cx.notify();
    }
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
enum VoiceEvent {
    Transcript(String),
    Error(String),
    Finished,
}

#[cfg(not(target_os = "windows"))]
struct Capture;

#[cfg(not(target_os = "windows"))]
impl Capture {
    fn start(
        _: VoiceSettings,
        _: tokio::sync::mpsc::UnboundedSender<VoiceEvent>,
    ) -> anyhow::Result<Self> {
        anyhow::bail!("Microphone dictation is available on Windows only.");
    }
}

#[cfg(target_os = "windows")]
use windows_capture::Capture;

#[cfg(target_os = "windows")]
mod windows_capture {
    use super::{VoiceEvent, VoiceSettings};
    use anyhow::{Result, bail};
    use multibot_core::voice::{has_speech, utterance_boundary};
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    use std::thread::JoinHandle;
    use tokio::sync::mpsc::UnboundedSender;

    type Handle = *mut c_void;
    const BUFFER_BYTES: usize = 6400; // 200 ms of 16 kHz mono PCM16.
    const HEADER_BYTES: u32 = std::mem::size_of::<WaveHeader>() as u32;

    #[repr(C, packed(1))]
    struct WaveFormat {
        tag: u16,
        channels: u16,
        samples_per_sec: u32,
        bytes_per_sec: u32,
        block_align: u16,
        bits_per_sample: u16,
        extra_size: u16,
    }

    #[repr(C)]
    #[derive(Default)]
    struct WaveHeader {
        data: *mut u8,
        buffer_length: u32,
        bytes_recorded: u32,
        user: usize,
        flags: u32,
        loops: u32,
        next: *mut WaveHeader,
        reserved: usize,
    }

    #[link(name = "winmm")]
    unsafe extern "system" {
        fn waveInOpen(
            input: *mut Handle,
            device: u32,
            format: *const WaveFormat,
            callback: usize,
            instance: usize,
            flags: u32,
        ) -> u32;
        fn waveInPrepareHeader(input: Handle, header: *mut WaveHeader, size: u32) -> u32;
        fn waveInUnprepareHeader(input: Handle, header: *mut WaveHeader, size: u32) -> u32;
        fn waveInAddBuffer(input: Handle, header: *mut WaveHeader, size: u32) -> u32;
        fn waveInStart(input: Handle) -> u32;
        fn waveInStop(input: Handle) -> u32;
        fn waveInReset(input: Handle) -> u32;
        fn waveInClose(input: Handle) -> u32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateEventW(
            attributes: *const c_void,
            manual_reset: i32,
            initial: i32,
            name: *const u16,
        ) -> Handle;
        fn SetEvent(event: Handle) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
        fn WaitForMultipleObjects(
            count: u32,
            handles: *const Handle,
            all: i32,
            timeout: u32,
        ) -> u32;
    }

    pub(super) struct Capture {
        stop: usize,
        cancelled: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl Capture {
        pub(super) fn cancel(&mut self) {
            self.cancelled.store(true, Ordering::Release);
        }

        pub(super) fn start(
            config: VoiceSettings,
            events: UnboundedSender<VoiceEvent>,
        ) -> Result<Self> {
            let stop = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) } as usize;
            if stop == 0 {
                bail!("Could not create the microphone stop event.");
            }
            let cancelled = Arc::new(AtomicBool::new(false));
            let (audio_tx, audio_rx) = mpsc::sync_channel::<Vec<u8>>(8);
            let http_cancelled = cancelled.clone();
            let http_events = events.clone();
            // This worker exists only for a dictation session, including its final request.
            let http = std::thread::Builder::new()
                .name("dictation-http".into())
                .spawn(move || {
                    while let Ok(pcm) = audio_rx.recv() {
                        if http_cancelled.load(Ordering::Acquire) || http_events.is_closed() {
                            break;
                        }
                        match multibot_core::voice::transcribe_pcm(&config, &pcm) {
                            Ok(text) => {
                                if http_events.send(VoiceEvent::Transcript(text)).is_err() {
                                    break;
                                }
                            }
                            Err(err) => {
                                let _ = http_events.send(VoiceEvent::Error(err.to_string()));
                                return;
                            }
                        }
                    }
                    let _ = http_events.send(VoiceEvent::Finished);
                });
            if http.is_err() {
                unsafe {
                    CloseHandle(stop as Handle);
                }
                bail!("Could not start the dictation request worker.");
            }
            let thread = std::thread::Builder::new()
                .name("dictation-mic".into())
                .spawn(move || {
                    if let Err(err) = record(stop as Handle, &audio_tx) {
                        let _ = events.send(VoiceEvent::Error(err.to_string()));
                    }
                });
            match thread {
                Ok(thread) => Ok(Self {
                    stop,
                    cancelled,
                    thread: Some(thread),
                }),
                Err(_) => {
                    cancelled.store(true, Ordering::Release);
                    unsafe {
                        CloseHandle(stop as Handle);
                    }
                    bail!("Could not start the microphone capture worker.");
                }
            }
        }
    }

    impl Drop for Capture {
        fn drop(&mut self) {
            // Wake a blocked capture immediately. No timer, polling, or sleeping thread.
            unsafe {
                SetEvent(self.stop as Handle);
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            unsafe {
                CloseHandle(self.stop as Handle);
            }
        }
    }

    struct Device {
        input: Handle,
        event: Handle,
        headers: Box<[WaveHeader; 4]>,
        buffers: Box<[[u8; BUFFER_BYTES]; 4]>,
        prepared: usize,
    }

    impl Drop for Device {
        fn drop(&mut self) {
            unsafe {
                if !self.input.is_null() {
                    waveInStop(self.input);
                    waveInReset(self.input);
                    for header in &mut self.headers[..self.prepared] {
                        waveInUnprepareHeader(self.input, header, HEADER_BYTES);
                    }
                    waveInClose(self.input);
                }
                CloseHandle(self.event);
            }
        }
    }

    fn enqueue(tx: &mpsc::SyncSender<Vec<u8>>, pcm: &mut Vec<u8>) -> Result<()> {
        if !has_speech(pcm) {
            pcm.clear();
            return Ok(());
        }
        tx.try_send(std::mem::take(pcm)).map_err(|_| {
            anyhow::anyhow!("Dictation could not keep up. Check your connection and try again.")
        })
    }

    fn record(stop: Handle, audio: &mpsc::SyncSender<Vec<u8>>) -> Result<()> {
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        if event.is_null() {
            bail!("Could not create the microphone audio event.");
        }
        let mut device = Device {
            input: null_mut(),
            event,
            headers: Box::new(std::array::from_fn(|_| WaveHeader::default())),
            buffers: Box::new([[0; BUFFER_BYTES]; 4]),
            prepared: 0,
        };
        let format = WaveFormat {
            tag: 1,
            channels: 1,
            samples_per_sec: 16000,
            bytes_per_sec: 32000,
            block_align: 2,
            bits_per_sample: 16,
            extra_size: 0,
        };
        unsafe {
            if waveInOpen(
                &mut device.input,
                u32::MAX,
                &format,
                event as usize,
                0,
                0x0005_0000,
            ) != 0
            {
                bail!(
                    "Cannot open the microphone at 16 kHz. Check your Windows microphone and privacy settings."
                );
            }
            for i in 0..4 {
                device.headers[i].data = device.buffers[i].as_mut_ptr();
                device.headers[i].buffer_length = BUFFER_BYTES as u32;
                if waveInPrepareHeader(device.input, &mut device.headers[i], HEADER_BYTES) != 0 {
                    bail!("Could not prepare the microphone audio buffer.");
                }
                device.prepared += 1;
                if waveInAddBuffer(device.input, &mut device.headers[i], HEADER_BYTES) != 0 {
                    bail!("Could not queue the microphone audio buffer.");
                }
            }
            if waveInStart(device.input) != 0 {
                bail!("Could not start the microphone.");
            }
            let handles = [stop, event];
            let mut pcm = Vec::with_capacity(14 * 32_000);
            let mut next = 0;
            loop {
                let result = WaitForMultipleObjects(2, handles.as_ptr(), 0, u32::MAX);
                if result == 0 {
                    waveInStop(device.input);
                    waveInReset(device.input);
                } else if result != 1 {
                    bail!("The microphone stopped responding.");
                }
                // Process in queue order, including partial buffers returned by reset.
                for _ in 0..4 {
                    let header = &mut device.headers[next];
                    if std::ptr::read_volatile(&header.flags) & 1 == 0 {
                        break;
                    }
                    let count = header.bytes_recorded as usize;
                    pcm.extend_from_slice(&device.buffers[next][..count.min(BUFFER_BYTES)]);
                    if result != 0 && waveInAddBuffer(device.input, header, HEADER_BYTES) != 0 {
                        bail!("Could not requeue the microphone audio buffer.");
                    }
                    next = (next + 1) % 4;
                    if result != 0 {
                        while let Some(boundary) = utterance_boundary(&pcm) {
                            let mut utterance = pcm.drain(..boundary).collect();
                            enqueue(audio, &mut utterance)?;
                        }
                    }
                }
                if result == 0 {
                    enqueue(audio, &mut pcm)?;
                    break;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{AppContext, TestAppContext, WindowOptions};
    use std::{sync::Arc, time::Duration};

    #[gpui_kit::test]
    fn recording_clock_is_cancelled_on_stop_and_cancel(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply_multibot_dark_theme(cx);
        });
        let runtime = Arc::new(
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        let (handle, view) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| MultibotView::new(runtime, window, cx))
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                assert!(v.voice.timer.is_none());
                v.begin_listening(window, cx);
                assert!(v.voice.timer.is_some());
                v.toggle_dictation(window, cx);
                assert!(v.voice.timer.is_none());
                assert_eq!(v.voice.phase, DictationPhase::Finishing);
                v.voice.cancel();
                v.begin_listening(window, cx);
                assert!(v.voice.timer.is_some());
                v.voice.cancel();
                assert!(v.voice.timer.is_none());
                assert_eq!(v.voice.phase, DictationPhase::Off);
                assert!(v.voice.started_at.is_none());
            });
        })
        .unwrap();
        cx.executor().advance_clock(Duration::from_secs(2));
        cx.run_until_parked();
        assert!(view.read_with(cx, |v, _| v.voice.timer.is_none()));
    }
}
