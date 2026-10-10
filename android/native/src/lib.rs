// SPDX-License-Identifier: GPL-3.0-or-later

//! The shared library the APK loads.
//!
//! It holds no code: `android_main` and the USB access are in rsttherm's `android` module, and
//! this crate exists only to link them into a cdylib.

// rustc links a dependency only if it is used; without this the library exports nothing.
use rsttherm as _;
