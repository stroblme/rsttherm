// SPDX-License-Identifier: GPL-3.0-or-later

//! The Android app: its entry point, and the USB access camera.rs gets from
//! nusb elsewhere. Android apps may not list or open `/dev/bus/usb`
//! themselves; `UsbManager` lists the devices and, once the user allows it,
//! hands over an open file descriptor for nusb to wrap.

use std::os::fd::{BorrowedFd, OwnedFd};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use android_activity::AndroidApp;
use eframe::egui::epaint::MarginF32;
use jni::objects::{JMap, JObject, JString, Reference};
use jni::refs::Global;
use jni::{Env, JavaVM, jni_sig, jni_str};
use nusb::MaybeFuture;

use crate::camera::Result;

/// For [`insets`], which the viewer asks for every frame.
static APP: OnceLock<AndroidApp> = OnceLock::new();

#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    let _ = APP.set(app.clone());
    // The viewer saves snapshots to the working directory, which is `/` here:
    // Download/rsttherm, which apps may write to without a permission from
    // Android 11 on (Pictures takes images only, not the CSV), else the app's
    // own folder. That is `<storage>/Android/data/<package>/files`, so the
    // storage holding Download is four levels up.
    let dir = app.external_data_path().map(|own| {
        let shared = own
            .ancestors()
            .nth(4)
            .map(|storage| storage.join("Download/rsttherm"));
        match shared {
            Some(shared) if std::fs::create_dir_all(&shared).is_ok() => shared,
            _ => own,
        }
    });
    if let Some(dir) = dir {
        let _ = std::env::set_current_dir(dir);
    }
    let options = eframe::NativeOptions {
        persistence_path: app.internal_data_path().map(|dir| dir.join("app.ron")),
        android_app: Some(app),
        ..Default::default()
    };
    // android-activity forwards stderr to logcat.
    if let Err(e) = crate::gui::run(None, options) {
        eprintln!("rsttherm: {e}");
    }
}

/// What the system bars and the camera cutout cover of the window, in pixels:
/// NativeActivity's content rect is the window less those.
pub fn insets() -> Option<MarginF32> {
    let app = APP.get()?;
    let window = app.native_window()?;
    let rect = app.content_rect();
    // Empty until the activity's first layout.
    if rect.right <= rect.left {
        return None;
    }
    Some(MarginF32 {
        left: rect.left as f32,
        top: rect.top as f32,
        right: (window.width() - rect.right) as f32,
        bottom: (window.height() - rect.bottom) as f32,
    })
}

/// When a field took the keyboard, and whether it has been asked for again.
static KEYBOARD: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

/// egui hides and at once re-shows the keyboard as a field takes focus, and
/// Android drops the second request, so the keyboard never appears. Once a
/// field has had focus for a moment, ask for it once more.
pub fn keep_keyboard(wanted: bool) {
    let mut keyboard = KEYBOARD.lock().unwrap();
    if !wanted {
        *keyboard = None;
        return;
    }
    let (since, asked) = keyboard.get_or_insert((Instant::now(), false));
    if !*asked && since.elapsed() > Duration::from_millis(300) {
        *asked = true;
        if let Some(app) = APP.get() {
            // Implicit, as winit asks, or its implicit hide would not close it.
            app.show_soft_input(true);
        }
    }
}

/// Stands in for nusb's [`MaybeFuture`], so camera.rs calls this module and
/// nusb alike.
pub struct Ready<T>(T);

impl<T> Ready<T> {
    pub fn wait(self) -> T {
        self.0
    }
}

/// The part of nusb's `DeviceInfo` camera.rs uses.
pub struct DeviceInfo {
    /// The usbfs path, `/dev/bus/usb/<bus>/<address>`, which Android also keys
    /// its device list by.
    name: String,
    vendor_id: u16,
    product_id: u16,
    serial_number: Option<String>,
}

/// Which device access was last asked for, so the streaming thread's retries
/// do not stack up dialogs.
static ASKED: Mutex<String> = Mutex::new(String::new());

pub fn list_devices() -> Ready<Result<std::vec::IntoIter<DeviceInfo>>> {
    Ready(
        with_service("usb", |env, _, usb| {
            let mut entries = device_list(env, usb)?.iter(env)?;
            let mut devices = Vec::new();
            while let Some(entry) = entries.next(env)? {
                let device = entry.value(env)?;
                let name = entry.key(env)?;
                let name = env.cast_local::<JString>(name)?;
                let id = |env: &mut Env, method| {
                    env.call_method(&device, method, jni_sig!(() -> jint), &[])?
                        .i()
                        .map(|id| id as u16)
                };
                devices.push(DeviceInfo {
                    name: name.try_to_string(env)?,
                    vendor_id: id(env, jni_str!("getVendorId"))?,
                    product_id: id(env, jni_str!("getProductId"))?,
                    serial_number: serial_number(env, &device),
                });
            }
            Ok(devices)
        })
        .map(Vec::into_iter),
    )
}

impl DeviceInfo {
    pub fn vendor_id(&self) -> u16 {
        self.vendor_id
    }

    pub fn product_id(&self) -> u16 {
        self.product_id
    }

    pub fn serial_number(&self) -> Option<&str> {
        self.serial_number.as_deref()
    }

    pub fn bus_id(&self) -> &str {
        self.name.rsplit('/').nth(1).unwrap_or_default()
    }

    pub fn device_address(&self) -> u8 {
        let address = self.name.rsplit('/').next();
        address.and_then(|a| a.parse().ok()).unwrap_or_default()
    }

    /// Asks the user for access the first time and fails until it is given;
    /// the streaming thread retries every second.
    pub fn open(&self) -> Ready<Result<nusb::Device>> {
        Ready(
            self.fd()
                .and_then(|fd| Ok(nusb::Device::from_fd(fd).wait()?)),
        )
    }

    fn fd(&self) -> Result<OwnedFd> {
        with_service("usb", |env, context, usb| {
            let name = env.new_string(&self.name)?;
            let device = device_list(env, usb)?
                .get(env, &name)?
                .ok_or("camera unplugged")?;
            let has_permission = env
                .call_method(
                    usb,
                    jni_str!("hasPermission"),
                    jni_sig!((device: android.hardware.usb.UsbDevice) -> boolean),
                    &[(&device).into()],
                )?
                .z()?;
            if !has_permission {
                let mut asked = ASKED.lock().unwrap();
                if *asked != self.name {
                    request_permission(env, context, usb, &device)?;
                    asked.clone_from(&self.name);
                }
                return Err("waiting for USB permission (replug to be asked again)".into());
            }
            let connection = env
                .call_method(
                    usb,
                    jni_str!("openDevice"),
                    jni_sig!((device: android.hardware.usb.UsbDevice) -> android.hardware.usb.UsbDeviceConnection),
                    &[(&device).into()],
                )?
                .l()?;
            if connection.is_null() {
                return Err("Android could not open the camera".into());
            }
            let fd = env
                .call_method(
                    &connection,
                    jni_str!("getFileDescriptor"),
                    jni_sig!(() -> jint),
                    &[],
                )?
                .i()?;
            // SAFETY: the connection keeps `fd` open until it is closed below.
            let fd = unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned();
            // ponytail: the copy keeps the device open, so the connection is
            // closed right away; keep it alive alongside the copy if a device
            // misbehaves once it is closed.
            env.call_method(&connection, jni_str!("close"), jni_sig!(() -> void), &[])?;
            Ok(fd?)
        })
    }
}

/// Runs `f` with this thread attached to the VM, passing the application
/// context and its system service `name`, such as the `UsbManager` for "usb".
fn with_service<T>(
    name: &str,
    f: impl FnOnce(&mut Env, &JObject, &JObject) -> Result<T>,
) -> Result<T> {
    let context: jni::sys::jobject = ndk_context::android_context().context().cast();
    JavaVM::singleton()?.attach_current_thread(|env| {
        // SAFETY: android-activity keeps this global reference to the
        // Application for the life of the process.
        let context = unsafe { env.as_cast_raw::<Global<JObject>>(&context)? };
        let name = env.new_string(name)?;
        let service = env
            .call_method(
                &*context,
                jni_str!("getSystemService"),
                jni_sig!((name: JString) -> JObject),
                &[(&name).into()],
            )?
            .l()?;
        f(env, &context, &service)
    })
}

/// Quarter turns clockwise the screen's content is drawn at, from the phone's
/// natural orientation: `Display.getRotation()`, read each frame since turning
/// the phone over, from one landscape to the other, changes nothing else.
pub fn display_turns() -> u8 {
    let rotation = with_service("window", |env, _, window| {
        let display = env
            .call_method(
                window,
                jni_str!("getDefaultDisplay"),
                jni_sig!(() -> android.view.Display),
                &[],
            )?
            .l()?;
        let rotation =
            env.call_method(&display, jni_str!("getRotation"), jni_sig!(() -> jint), &[])?;
        Ok(rotation.i()? as u8)
    });
    rotation.unwrap_or(0)
}

/// `UsbManager.getDeviceList()`, keyed by [`DeviceInfo::name`].
fn device_list<'local>(env: &mut Env<'local>, usb: &JObject) -> Result<JMap<'local>> {
    let sig = jni_sig!(() -> java.util.HashMap);
    let list = env
        .call_method(usb, jni_str!("getDeviceList"), sig, &[])?
        .l()?;
    Ok(JMap::cast_local(env, list)?)
}

/// `None` until the app may access the device: Android 10 and later throw a
/// `SecurityException` before that.
fn serial_number(env: &mut Env, device: &JObject) -> Option<String> {
    let serial = env
        .call_method(
            device,
            jni_str!("getSerialNumber"),
            jni_sig!(() -> JString),
            &[],
        )
        .and_then(|serial| env.cast_local::<JString>(serial.l()?))
        .and_then(|serial| serial.try_to_string(env));
    env.exception_clear();
    serial.ok()
}

/// Shows Android's dialog for access to `device`. Nothing listens for the
/// answer: [`DeviceInfo::open`] checks again on its next attempt.
fn request_permission(
    env: &mut Env,
    context: &JObject,
    usb: &JObject,
    device: &JObject,
) -> Result<()> {
    let package = env
        .call_method(
            context,
            jni_str!("getPackageName"),
            jni_sig!(() -> JString),
            &[],
        )?
        .l()?;
    let package = env.cast_local::<JString>(package)?;
    let action = env.new_string(format!("{}.USB_PERMISSION", package.try_to_string(env)?))?;
    let intent = env.new_object(
        jni_str!("android/content/Intent"),
        jni_sig!((action: JString)),
        &[(&action).into()],
    )?;
    // Android 14 refuses a mutable PendingIntent for an implicit Intent.
    env.call_method(
        &intent,
        jni_str!("setPackage"),
        jni_sig!((package: JString) -> android.content.Intent),
        &[(&package).into()],
    )?;
    // FLAG_MUTABLE, since the system adds the device and the answer to it.
    const FLAG_MUTABLE: i32 = 0x0200_0000;
    let pending = env
        .call_static_method(
            jni_str!("android/app/PendingIntent"),
            jni_str!("getBroadcast"),
            jni_sig!((context: android.content.Context, code: jint, intent: android.content.Intent, flags: jint) -> android.app.PendingIntent),
            &[context.into(), 0.into(), (&intent).into(), FLAG_MUTABLE.into()],
        )?
        .l()?;
    env.call_method(
        usb,
        jni_str!("requestPermission"),
        jni_sig!((device: android.hardware.usb.UsbDevice, intent: android.app.PendingIntent)),
        &[device.into(), (&pending).into()],
    )?;
    Ok(())
}
