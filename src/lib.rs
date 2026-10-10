// SPDX-License-Identifier: GPL-3.0-or-later

//! The Thermal Master P1/P3 driver and everything built on it, shared by the
//! `rsttherm` binary and the Android app in `android/native`.

#[cfg(target_os = "android")]
mod android;
pub mod camera;
#[cfg(feature = "gui")]
pub mod gui;
pub mod http;
pub mod live;
pub mod thermal;
