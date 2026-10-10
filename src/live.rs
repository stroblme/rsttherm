// SPDX-License-Identifier: GPL-3.0-or-later

//! A camera streamed on a background thread and shared by everything in the
//! process. The USB interfaces can only be claimed once, so the viewer and the
//! HTTP API both go through one of these rather than opening the camera
//! themselves.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::camera::{self, Frame, Result};
use crate::thermal::{Correction, Image};

enum Cmd {
    Shutter,
    /// Switch to the camera with this [`camera::Device::id`].
    Select(String),
}

pub struct Live {
    frame: Mutex<Option<Arc<Frame>>>,
    status: Mutex<String>,
    /// Applied by [`Live::image`]; the viewer keeps it in step with its sliders
    /// so the HTTP API reports what the operator sees.
    pub correction: Mutex<Correction>,
    /// Id of the camera currently open.
    device: Mutex<Option<String>>,
    tx: Sender<Cmd>,
}

impl Live {
    /// Starts streaming from the camera `spec` selects, retrying until one is
    /// plugged in and reconnecting after it is unplugged.
    pub fn start(spec: Option<String>, correction: Correction) -> Arc<Self> {
        let (tx, rx) = mpsc::channel();
        let live = Arc::new(Self {
            frame: Mutex::new(None),
            status: Mutex::new("connecting...".into()),
            correction: Mutex::new(correction),
            device: Mutex::new(None),
            tx,
        });
        let shared = Arc::clone(&live);
        thread::spawn(move || {
            let mut spec = spec;
            loop {
                // A camera picked while none could be opened still counts.
                for cmd in rx.try_iter() {
                    if let Cmd::Select(id) = cmd {
                        spec = Some(id);
                    }
                }
                let result = stream(&shared, spec.as_deref(), &rx);
                *shared.frame.lock().unwrap() = None;
                *shared.device.lock().unwrap() = None;
                match result {
                    Ok(selected) => spec = Some(selected),
                    Err(e) => {
                        shared.set_status(e.to_string());
                        thread::sleep(Duration::from_secs(1));
                    }
                }
            }
        });
        live
    }

    pub fn frame(&self) -> Option<Arc<Frame>> {
        self.frame.lock().unwrap().clone()
    }

    pub fn image(&self) -> Result<Image> {
        let frame = self.frame().ok_or_else(|| self.status())?;
        Ok(Image::new(&frame, &self.correction.lock().unwrap()))
    }

    /// Empty while frames are flowing, otherwise what went wrong.
    pub fn status(&self) -> String {
        self.status.lock().unwrap().clone()
    }

    fn set_status(&self, status: impl Into<String>) {
        *self.status.lock().unwrap() = status.into();
    }

    /// The id of the camera being streamed, if one is open.
    pub fn device(&self) -> Option<String> {
        self.device.lock().unwrap().clone()
    }

    /// Runs a shutter calibration on the streaming thread.
    pub fn shutter(&self) {
        let _ = self.tx.send(Cmd::Shutter);
    }

    /// Closes the current camera and streams from the one with this id instead.
    pub fn select(&self, id: String) {
        let _ = self.tx.send(Cmd::Select(id));
    }
}

/// Streams until something fails, or until another camera is selected, whose id
/// it then returns.
fn stream(live: &Live, spec: Option<&str>, rx: &Receiver<Cmd>) -> Result<String> {
    let mut cam = camera::open(spec)?;
    *live.device.lock().unwrap() = Some(cam.id.clone());
    live.set_status("starting stream...");
    cam.start()?;
    loop {
        for cmd in rx.try_iter() {
            match cmd {
                Cmd::Shutter => cam.shutter()?,
                Cmd::Select(id) => return Ok(id),
            }
        }
        let frame = cam.read_frame()?;
        *live.frame.lock().unwrap() = Some(Arc::new(frame));
        live.set_status("");
    }
}
