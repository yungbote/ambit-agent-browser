//! All Pulse objects belong to this worker thread. Its poll has a finite wait;
//! no thread-unsafe handle crosses into Tokio or a browser-input operation.
use super::{AudioError, CHANNELS, FRAME_BYTES, SAMPLE_RATE};
use libpulse_binding as pulse;
use pulse::{
    context::{Context, FlagSet as ContextFlags, State as ContextState},
    def::BufferAttr,
    mainloop::standard::Mainloop,
    sample::{Format, Spec},
    stream::{FlagSet, Latency, PeekResult, State, Stream},
    time::MicroSeconds,
};
use std::path::Path;
use std::time::{Duration, Instant};

pub(super) struct Connection {
    context: Context,
    mainloop: Mainloop,
}
impl Connection {
    pub(super) fn connect(
        socket: &Path,
        cookie: &Path,
        canceled: impl Fn() -> bool,
    ) -> Result<Self, AudioError> {
        let mainloop = Mainloop::new().ok_or(AudioError::Unavailable)?;
        let mut context =
            Context::new(&mainloop, "Ambit browser output").ok_or(AudioError::Unavailable)?;
        context
            .load_cookie_from_file(cookie.to_str().ok_or(AudioError::Unavailable)?)
            .map_err(|_| AudioError::Unavailable)?;
        context
            .connect(
                Some(&format!("unix:{}", socket.display())),
                ContextFlags::NOAUTOSPAWN,
                None,
            )
            .map_err(|_| AudioError::Unavailable)?;
        let mut connection = Self { context, mainloop };
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if canceled() || Instant::now() >= deadline {
                return Err(AudioError::Unavailable);
            }
            match connection.context.get_state() {
                ContextState::Ready => return Ok(connection),
                ContextState::Failed | ContextState::Terminated => {
                    return Err(AudioError::Unavailable)
                }
                _ => connection.tick()?,
            }
        }
    }
    pub(super) fn tick(&mut self) -> Result<(), AudioError> {
        self.mainloop
            .prepare(Some(MicroSeconds(5_000)))
            .map_err(|_| AudioError::Unavailable)?;
        self.mainloop.poll().map_err(|_| AudioError::Unavailable)?;
        self.mainloop
            .dispatch()
            .map_err(|_| AudioError::Unavailable)?;
        Ok(())
    }
    pub(super) fn record(&mut self, canceled: impl Fn() -> bool) -> Result<Stream, AudioError> {
        let spec = Spec {
            format: Format::S16le,
            channels: CHANNELS,
            rate: SAMPLE_RATE,
        };
        let mut stream = Stream::new(&mut self.context, "Browser output", &spec, None)
            .ok_or(AudioError::Unavailable)?;
        let attr = BufferAttr {
            maxlength: (FRAME_BYTES * 6) as u32,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: FRAME_BYTES as u32,
        };
        stream
            .connect_record(
                Some("ambit.monitor"),
                Some(&attr),
                FlagSet::ADJUST_LATENCY | FlagSet::AUTO_TIMING_UPDATE | FlagSet::INTERPOLATE_TIMING,
            )
            .map_err(|_| AudioError::Unavailable)?;
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if canceled() || Instant::now() >= deadline {
                return Err(AudioError::Unavailable);
            }
            match stream.get_state() {
                State::Ready => return Ok(stream),
                State::Failed | State::Terminated => return Err(AudioError::Unavailable),
                _ => self.tick()?,
            }
        }
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        self.context.disconnect();
    }
}

pub(super) enum Capture {
    Empty,
    Samples { bytes: Vec<u8>, ts: u64 },
    Gap,
}
pub(super) fn read(stream: &mut Stream) -> Result<Capture, AudioError> {
    if stream.get_state() != State::Ready {
        return Err(AudioError::Unavailable);
    }
    // Pulse's record latency includes queued source samples; it can be negative for monitors.
    // Never stamp receipt time as source time when timing information is absent.
    let now = crate::native::stream::monotonic_us();
    let ts = match stream.get_latency().map_err(|_| AudioError::Unavailable)? {
        Latency::Positive(value) => Some(now.saturating_sub(value.0)),
        Latency::Negative(value) => Some(now.saturating_add(value.0)),
        Latency::None => None,
    };
    let captured = match stream.peek().map_err(|_| AudioError::Unavailable)? {
        PeekResult::Empty => return Ok(Capture::Empty),
        PeekResult::Hole(_) => Capture::Gap,
        PeekResult::Data(bytes) if bytes.len() <= FRAME_BYTES * 6 && bytes.len() % 4 == 0 => {
            match ts {
                Some(ts) => Capture::Samples {
                    bytes: bytes.to_vec(),
                    ts,
                },
                None => Capture::Gap,
            }
        }
        PeekResult::Data(_) => Capture::Gap,
    };
    stream.discard().map_err(|_| AudioError::Unavailable)?;
    Ok(captured)
}
