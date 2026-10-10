// SPDX-License-Identifier: GPL-3.0-or-later

//! USB driver for Thermal Master P1/P3 thermal cameras.
//!
//! The cameras are vendor-class USB devices rather than UVC, so there is no
//! kernel driver to go through: everything is control and bulk transfers from
//! user space via nusb.
//!
//! The protocol was reverse-engineered from USB captures of the vendor's
//! Windows tool by [p3-ir-camera](https://github.com/jvdillon/p3-ir-camera)
//! (Apache-2.0); see its `P3_PROTOCOL.md`. Command bytes, the start-up sequence
//! with its delays, and the frame resynchronisation all follow that project.

use std::fmt;
use std::thread::sleep;
use std::time::Duration;

use nusb::transfer::{Bulk, ControlIn, ControlOut, ControlType, In, Recipient};
use nusb::{Endpoint, Interface, MaybeFuture};
// Android apps may not enumerate or open USB devices themselves.
#[cfg(target_os = "android")]
use crate::android::{self as usb, DeviceInfo};
#[cfg(not(target_os = "android"))]
use nusb::{self as usb, DeviceInfo};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const VID: u16 = 0x3474;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Model {
    pub pid: u16,
    pub name: &'static str,
    pub width: usize,
    pub height: usize,
}

const MODELS: [Model; 2] = [
    Model {
        pid: 0x45a2,
        name: "P3",
        width: 256,
        height: 192,
    },
    Model {
        pid: 0x45c2,
        name: "P1",
        width: 160,
        height: 120,
    },
];

const TIMEOUT: Duration = Duration::from_secs(1);
/// Frames arrive every 40 ms, but the first one after start-up takes ~1.5 s.
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);
const FRAME_ENDPOINT: u8 = 0x81;
/// Every frame is wrapped in a 12-byte start and end marker; see [`assemble`].
const MARKER: usize = 12;
/// Plain 16 KiB reads, as in the reference driver. Frame boundaries come from
/// the end-marker transfer in [`assemble`], so reads need not match the vendor
/// tool's transfer sizes.
const CHUNK: usize = 16 * 1024;
/// Transfers kept queued so the camera never waits on us between reads.
const IN_FLIGHT: usize = 4;

/// Raw readings are apparent temperature in 1/64 K.
pub const KELVIN: f32 = 273.15;

pub fn celsius(raw: u16) -> f32 {
    raw as f32 / 64.0 - KELVIN
}

// ---- commands -----------------------------------------------------------

const READ_REGISTER: [u8; 2] = [0x01, 0x01];
const STREAM: [u8; 2] = [0x01, 0x2f];
const SHUTTER: [u8; 2] = [0x01, 0x36];

/// The 18-byte command block: kind, parameter, register, six reserved bytes,
/// response length, two more reserved bytes, then a CRC16 over the first 16. The camera ignores
/// the CRC, but the vendor tool sends it, so we do too.
fn command(kind: [u8; 2], param: u8, register: u16, response_len: u16) -> [u8; 18] {
    let mut cmd = [0u8; 18];
    cmd[..2].copy_from_slice(&kind);
    cmd[2] = param;
    cmd[4..6].copy_from_slice(&register.to_le_bytes());
    cmd[12..14].copy_from_slice(&response_len.to_le_bytes());
    let crc = crc16(&cmd[..16]);
    cmd[16..].copy_from_slice(&crc.to_le_bytes());
    cmd
}

/// CRC-16/XMODEM: polynomial 0x1021, initial value 0.
fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

// ---- devices ------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// The serial number, or bus and address when the camera reports none.
    pub id: String,
    pub name: String,
    pub vid_pid: (u16, u16),
}

impl fmt::Display for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (vid, pid) = self.vid_pid;
        write!(f, "{}\t{vid:04x}:{pid:04x}\t{}", self.id, self.name)
    }
}

fn model_of(info: &DeviceInfo) -> Option<Model> {
    (info.vendor_id() == VID)
        .then(|| MODELS.into_iter().find(|m| m.pid == info.product_id()))
        .flatten()
}

fn device_of(info: &DeviceInfo, model: Model) -> Device {
    // The P3's serial descriptor ends in a few bytes of junk after the number.
    let serial = info
        .serial_number()
        .and_then(|s| s.split(|c: char| !c.is_ascii_graphic()).next())
        .filter(|s| !s.is_empty());
    let id = match serial {
        Some(serial) => serial.to_string(),
        None => format!("{}-{}", info.bus_id(), info.device_address()),
    };
    Device {
        id,
        name: model.name.to_string(),
        vid_pid: (VID, model.pid),
    }
}

fn cameras() -> Result<Vec<(DeviceInfo, Device, Model)>> {
    Ok(usb::list_devices()
        .wait()?
        .filter_map(|info| {
            let model = model_of(&info)?;
            let device = device_of(&info, model);
            Some((info, device, model))
        })
        .collect())
}

pub fn list_devices() -> Result<Vec<Device>> {
    Ok(cameras()?.into_iter().map(|(_, d, _)| d).collect())
}

/// What a `--device` argument selects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spec<'a> {
    VidPid(u16, u16),
    /// Case-insensitive substring of the id (serial number) or model name.
    Text(&'a str),
}

pub fn parse_spec(spec: &str) -> Result<Spec<'_>> {
    if spec.is_empty() {
        return Err("empty device spec".into());
    }
    match spec.split_once(':') {
        // A colon means the user meant VID:PID, so a malformed one is an error
        // rather than a substring that would never match.
        Some((vid, pid)) => {
            let parse = |s: &str| u16::from_str_radix(s, 16).ok();
            match (parse(vid), parse(pid)) {
                (Some(vid), Some(pid)) => Ok(Spec::VidPid(vid, pid)),
                _ => Err(format!("'{spec}' is not a hex VID:PID").into()),
            }
        }
        None => Ok(Spec::Text(spec)),
    }
}

impl Spec<'_> {
    pub fn matches(&self, dev: &Device) -> bool {
        match *self {
            Spec::VidPid(vid, pid) => dev.vid_pid == (vid, pid),
            Spec::Text(text) => {
                let text = text.to_lowercase();
                dev.id.to_lowercase().contains(&text) || dev.name.to_lowercase().contains(&text)
            }
        }
    }
}

// ---- the camera ---------------------------------------------------------

/// One thermal frame as the camera sent it.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub width: usize,
    pub height: usize,
    /// Apparent temperature per pixel in 1/64 K, row-major.
    pub raw: Vec<u16>,
}

pub struct Camera {
    pub model: Model,
    /// [`Device::id`], which reopens this camera when passed to [`open`].
    pub id: String,
    /// Interface 0 carries the commands, interface 1 the frames.
    control: Interface,
    stream: Interface,
    frames: Option<Endpoint<Bulk, In>>,
    pending: Vec<u8>,
}

/// Why opening usually fails: on Linux, no access without the udev rule.
#[cfg(not(target_os = "android"))]
const OPEN_HINT: &str = " (is the udev rule installed? see `make udev`)";
#[cfg(target_os = "android")]
const OPEN_HINT: &str = "";

/// Open a camera. `None` picks the first one found.
pub fn open(spec: Option<&str>) -> Result<Camera> {
    let spec = spec.map(parse_spec).transpose()?;
    let (info, device, model) = cameras()?
        .into_iter()
        .find(|(_, d, _)| spec.as_ref().is_none_or(|s| s.matches(d)))
        .ok_or("no thermal camera found")?;
    let usb = info
        .open()
        .wait()
        .map_err(|e| format!("{}: {e}{OPEN_HINT}", device.id))?;
    Ok(Camera {
        model,
        id: device.id,
        control: usb.detach_and_claim_interface(0).wait()?,
        stream: usb.detach_and_claim_interface(1).wait()?,
        frames: None,
        pending: Vec::new(),
    })
}

impl Camera {
    fn send(&self, cmd: &[u8; 18]) -> Result<()> {
        let out = ControlOut {
            control_type: ControlType::Vendor,
            recipient: Recipient::Interface,
            request: 0x20,
            value: 0,
            index: 0,
            data: cmd,
        };
        self.control.control_out(out, TIMEOUT).wait()?;
        self.ack()
    }

    fn control_in(&self, request: u8, length: u16) -> Result<Vec<u8>> {
        let req = ControlIn {
            control_type: ControlType::Vendor,
            recipient: Recipient::Interface,
            request,
            value: 0,
            index: 0,
            length,
        };
        Ok(self.control.control_in(req, TIMEOUT).wait()?)
    }

    /// The vendor tool reads this one-byte status after every transfer. The
    /// camera works without it, but less reliably.
    fn ack(&self) -> Result<()> {
        self.control_in(0x22, 1).map(drop)
    }

    fn query(&self, cmd: &[u8; 18], len: u16) -> Result<Vec<u8>> {
        self.send(cmd)?;
        let data = self.control_in(0x21, len)?;
        self.ack()?;
        Ok(data)
    }

    fn register(&self, register: u16, len: u16) -> Result<String> {
        let data = self.query(&command(READ_REGISTER, 0x81, register, len), len)?;
        let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
        Ok(String::from_utf8_lossy(&data[..end]).trim().to_string())
    }

    pub fn info(&self) -> Result<Vec<(&'static str, String)>> {
        [
            ("model", 0x01, 30),
            ("firmware", 0x02, 12),
            ("hardware", 0x0a, 64),
            ("part", 0x06, 64),
            ("serial", 0x07, 64),
        ]
        .into_iter()
        .map(|(name, reg, len)| Ok((name, self.register(reg, len)?)))
        .collect()
    }

    fn start_command(&self) -> Result<()> {
        // Answers 0x01 from idle and '5' when the camera was already streaming;
        // both are fine.
        self.query(&command(STREAM, 0x81, 0, 1), 1).map(drop)
    }

    /// The vendor tool's start-up sequence, delays included: the camera sends
    /// nothing if the second start command comes too early. Takes ~3 s.
    pub fn start(&mut self) -> Result<()> {
        self.start_command()?;
        sleep(Duration::from_secs(1));
        self.stream.set_alt_setting(1).wait()?;
        let enable = ControlOut {
            control_type: ControlType::Vendor,
            recipient: Recipient::Device,
            request: 0xee,
            value: 0,
            index: 1,
            data: &[],
        };
        self.stream.control_out(enable, TIMEOUT).wait()?;
        sleep(Duration::from_secs(2));
        // The vendor tool also has a read pending before its second start.
        let mut ep = self.stream.endpoint::<Bulk, In>(FRAME_ENDPOINT)?;
        for _ in 0..IN_FLIGHT {
            let buf = ep.allocate(CHUNK);
            ep.submit(buf);
        }
        self.frames = Some(ep);
        self.start_command()
    }

    /// Blocks until the next complete frame. Call [`Self::start`] first.
    pub fn read_frame(&mut self) -> Result<Frame> {
        let model = self.model;
        let ep = self.frames.as_mut().ok_or("stream not started")?;
        loop {
            let done = ep
                .wait_next_complete(FRAME_TIMEOUT)
                .ok_or("timed out waiting for a frame")?;
            done.status?;
            let complete = assemble(&mut self.pending, &done.buffer, frame_len(model));
            let mut buf = done.buffer;
            buf.clear();
            ep.submit(buf);
            if complete {
                let frame = parse(&self.pending, model);
                self.pending.clear();
                if let Some(frame) = frame {
                    return Ok(frame);
                }
            }
        }
    }

    /// Flat-field (NUC) calibration: the shutter closes with an audible click.
    /// The camera also does this by itself about every 90 s. The frame after it
    /// arrives malformed and is dropped by the resynchronisation in
    /// [`assemble`].
    pub fn shutter(&self) -> Result<()> {
        self.send(&command(SHUTTER, 0x43, 0, 0))
    }
}

// ---- frames -------------------------------------------------------------

/// Start marker, IR brightness rows, two metadata rows, the thermal rows, end
/// marker. Pixels are little-endian u16.
fn frame_len(m: Model) -> usize {
    MARKER + 2 * m.width * (2 * m.height + 2) + MARKER
}

/// Feeds one bulk transfer into the frame being collected in `pending`, and
/// returns true once it holds a whole frame.
///
/// A frame always ends with a transfer of just the 12-byte end marker, and the
/// read size never lines up with the frame size, so that transfer arriving
/// exactly at the end is the only way to finish one. Anywhere else, we joined
/// mid-frame (on start-up, or after the malformed frame following a shutter)
/// and start over; the next transfer then begins a fresh frame.
fn assemble(pending: &mut Vec<u8>, data: &[u8], len: usize) -> bool {
    let end = pending.len() + data.len();
    if data.len() == MARKER && end == len {
        pending.extend_from_slice(data);
        return true;
    }
    if data.len() == MARKER || end >= len {
        pending.clear();
    } else {
        pending.extend_from_slice(data);
    }
    false
}

/// Checks the markers and cuts the thermal rows out of an assembled frame.
///
/// Marker layout: length (12), sync byte, a frame counter shared by the start
/// and end marker, then two more counters. The sync byte alternates 0x8c/0x8d
/// on start markers and 0x8e/0x8f on end markers.
fn parse(buf: &[u8], m: Model) -> Option<Frame> {
    let start = &buf[..MARKER];
    let end = &buf[buf.len() - MARKER..];
    let valid = start[0] == MARKER as u8
        && end[0] == MARKER as u8
        && start[1] & !1 == 0x8c
        && end[1] & !1 == 0x8e
        && start[2..6] == end[2..6];
    if !valid {
        return None;
    }
    let rows = MARKER + 2 * m.width * (m.height + 2);
    let thermal = &buf[rows..rows + 2 * m.width * m.height];
    Some(Frame {
        width: m.width,
        height: m.height,
        raw: thermal
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&b| u16::from_le_bytes(b))
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Byte for byte what the vendor tool sends, CRC included, as captured by
    /// p3-ir-camera.
    #[test]
    fn builds_the_captured_commands() {
        for (cmd, captured) in [
            (
                command(READ_REGISTER, 0x81, 0x01, 30),
                "0101810001000000000000001e0000004f90",
            ),
            (
                command(READ_REGISTER, 0x81, 0x02, 12),
                "0101810002000000000000000c0000001f63",
            ),
            (
                command(READ_REGISTER, 0x81, 0x07, 64),
                "01018100070000000000000040000000104c",
            ),
            (
                command(STREAM, 0x81, 0, 1),
                "012f81000000000000000000010000004930",
            ),
            // Low and high gain. Not exposed: low gain reads ~57 K cold at room
            // temperature on firmware 00.00.02.18 and no conversion is known.
            (
                command(STREAM, 0x41, 0, 0),
                "012f41000000000000000000000000003c3a",
            ),
            (
                command(STREAM, 0x41, 1, 0),
                "012f41000100000000000000000000004939",
            ),
            (
                command(SHUTTER, 0x43, 0, 0),
                "01364300000000000000000000000000cd0b",
            ),
        ] {
            assert_eq!(cmd.to_vec(), hex(captured), "{captured}");
        }
    }

    const TINY: Model = Model {
        pid: 0,
        name: "test",
        width: 4,
        height: 2,
    };

    fn marker(sync: u8, counter: u32) -> Vec<u8> {
        let mut m = vec![12, sync];
        m.extend_from_slice(&counter.to_le_bytes());
        m.extend_from_slice(&[0; 6]);
        m
    }

    /// A frame whose thermal pixel `i` reads `1000 + i`, split into the
    /// transfers the camera would send for a read size of `chunk`.
    fn transfers(counter: u32, chunk: usize) -> Vec<Vec<u8>> {
        let (w, h) = (TINY.width, TINY.height);
        let mut body = marker(0x8c | (counter as u8 & 1), counter);
        body.extend(std::iter::repeat_n(0xaa, 2 * w * (h + 2)));
        for i in 0..(w * h) as u16 {
            body.extend_from_slice(&(1000 + i).to_le_bytes());
        }
        let mut out: Vec<Vec<u8>> = body.chunks(chunk).map(<[u8]>::to_vec).collect();
        out.push(marker(0x8e | (counter as u8 & 1), counter));
        out
    }

    fn run(stream: &[Vec<u8>]) -> Vec<Frame> {
        let mut pending = Vec::new();
        let mut frames = Vec::new();
        for data in stream {
            if assemble(&mut pending, data, frame_len(TINY)) {
                frames.extend(parse(&pending, TINY));
                pending.clear();
            }
        }
        frames
    }

    #[test]
    fn assembles_frames_from_transfers() {
        let stream: Vec<_> = (0..3).flat_map(|n| transfers(n, 20)).collect();
        let frames = run(&stream);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].raw, (1000..1008).collect::<Vec<u16>>());
    }

    #[test]
    fn resynchronises_after_joining_mid_frame() {
        // Join in the middle of frame 0: it is dropped, frame 1 comes through.
        let mut stream = transfers(0, 20);
        stream.drain(..2);
        stream.extend(transfers(1, 20));
        assert_eq!(run(&stream).len(), 1);

        // An oversized frame (as after a shutter) is dropped, the next one kept.
        let mut stream = transfers(0, 20);
        stream.insert(1, vec![0; 16]);
        stream.extend(transfers(1, 20));
        assert_eq!(run(&stream).len(), 1);
    }

    #[test]
    fn rejects_frames_with_mismatched_markers() {
        let mut stream = transfers(0, 20);
        *stream.last_mut().unwrap() = marker(0x8e, 7);
        assert!(run(&stream).is_empty());
    }

    #[test]
    fn frame_sizes_match_the_protocol() {
        // 197,632 bytes of pixels for the P3, plus the two markers.
        assert_eq!(frame_len(MODELS[0]), 197_632 + 24);
        assert_eq!(frame_len(MODELS[1]), 77_440 + 24);
    }

    #[test]
    fn converts_raw_readings() {
        assert!((celsius(((25.0 + KELVIN) * 64.0) as u16) - 25.0).abs() < 0.02);
    }

    #[test]
    fn parses_device_specs() {
        assert_eq!(
            parse_spec("3474:45a2").unwrap(),
            Spec::VidPid(0x3474, 0x45a2)
        );
        assert_eq!(parse_spec("P3").unwrap(), Spec::Text("P3"));
        for bad in ["", "3474:zzzz", "3474:", ":"] {
            assert!(parse_spec(bad).is_err(), "{bad:?} should not parse");
        }

        let dev = Device {
            id: "P3025043DG042100016".into(),
            name: "P3".into(),
            vid_pid: (0x3474, 0x45a2),
        };
        assert!(parse_spec("3474:45a2").unwrap().matches(&dev));
        assert!(!parse_spec("3474:45c2").unwrap().matches(&dev));
        assert!(parse_spec("p3").unwrap().matches(&dev));
        assert!(parse_spec("100016").unwrap().matches(&dev));
        assert!(!parse_spec("p1").unwrap().matches(&dev));
    }
}
