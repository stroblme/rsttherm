// SPDX-License-Identifier: GPL-3.0-or-later

//! rsttherm -- Thermal Master P1/P3 USB thermal cameras: a CLI, an HTTP API and
//! a live viewer sharing one driver.

mod camera;
#[cfg(feature = "gui")]
mod gui;
mod http;
mod live;
mod thermal;

use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use thermal::Correction;

/// Same split as rstptz: 0 ok, 8 bad syntax, 16 command failed.
const EXIT_USAGE: u8 = 8;
const EXIT_FAILED: u8 = 16;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Camera to use: a hex VID:PID, or part of its serial number or model name
    #[arg(short, long, global = true, value_name = "DEVICE", value_parser = valid_spec)]
    device: Option<String>,

    #[command(subcommand)]
    command: Command,
}

/// Reject a malformed spec while parsing, so it exits 8 like other bad syntax
/// rather than 16 as if the camera had failed.
fn valid_spec(spec: &str) -> Result<String, String> {
    camera::parse_spec(spec)
        .map(|_| spec.to_string())
        .map_err(|e| e.to_string())
}

#[derive(Args)]
struct CorrectionArgs {
    /// Surface emissivity, 0.1-1
    #[arg(short, long, default_value_t = Correction::DEFAULT.emissivity, value_parser = emissivity)]
    emissivity: f32,
    /// Reflected apparent temperature in °C, usually the room's
    #[arg(long, default_value_t = Correction::DEFAULT.reflected, allow_negative_numbers = true)]
    reflected: f32,
    /// Calibration offset in °C, added to every reading
    #[arg(long, default_value_t = Correction::DEFAULT.offset, allow_negative_numbers = true)]
    offset: f32,
}

impl From<CorrectionArgs> for Correction {
    fn from(a: CorrectionArgs) -> Self {
        Self {
            emissivity: a.emissivity,
            reflected: a.reflected,
            offset: a.offset,
        }
    }
}

fn emissivity(text: &str) -> Result<f32, String> {
    text.parse::<f32>()
        .ok()
        .filter(|e| (0.1..=1.0).contains(e))
        .ok_or_else(|| format!("emissivity must be 0.1-1, got '{text}'"))
}

fn point(text: &str) -> Result<(usize, usize), String> {
    text.split_once(',')
        .and_then(|(x, y)| Some((x.trim().parse().ok()?, y.trim().parse().ok()?)))
        .ok_or_else(|| format!("expected X,Y, got '{text}'"))
}

#[derive(Subcommand)]
enum Command {
    /// List connected cameras
    List,
    /// Print model, firmware and serial number
    Info,
    /// Read one frame and print min, max, mean and centre temperature in °C
    Measure {
        /// Also print the pixel at X,Y (sensor coordinates, 0,0 top left)
        #[arg(short, long, value_name = "X,Y", value_parser = point)]
        point: Option<(usize, usize)>,
        #[command(flatten)]
        correction: CorrectionArgs,
    },
    /// Run a shutter / flat-field (NUC) calibration
    Shutter,
    /// Stream and serve the HTTP API
    Serve {
        #[arg(long, default_value_t = http::DEFAULT_PORT)]
        port: u16,
        #[arg(long, default_value = http::DEFAULT_BIND)]
        bind: String,
        #[command(flatten)]
        correction: CorrectionArgs,
    },
    /// Run the viewer (also serves the HTTP API)
    #[cfg(feature = "gui")]
    Gui,
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            // --help and --version come back as errors too, but aren't failures.
            return if e.use_stderr() {
                ExitCode::from(EXIT_USAGE)
            } else {
                ExitCode::SUCCESS
            };
        }
    };

    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rsttherm: {e}");
            ExitCode::from(EXIT_FAILED)
        }
    }
}

fn run(cli: Cli) -> camera::Result<()> {
    let device = cli.device;
    match cli.command {
        Command::List => {
            for found in camera::list_devices()? {
                println!("{found}");
            }
        }
        Command::Info => {
            for (name, value) in camera::open(device.as_deref())?.info()? {
                println!("{name}\t{value}");
            }
        }
        Command::Measure { point, correction } => {
            let mut cam = camera::open(device.as_deref())?;
            cam.start()?;
            let frame = cam.read_frame()?;
            let image = thermal::Image::new(&frame, &correction.into());
            print!("{}", image.report(point)?);
        }
        Command::Shutter => camera::open(device.as_deref())?.shutter()?,
        Command::Serve {
            port,
            bind,
            correction,
        } => http::serve(&live::Live::start(device, correction.into()), &bind, port)?,
        #[cfg(feature = "gui")]
        Command::Gui => gui::run(device)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn accepts_the_documented_commands() {
        for args in [
            vec!["rsttherm", "list"],
            vec!["rsttherm", "info"],
            vec!["rsttherm", "-d", "3474:45a2", "measure"],
            vec!["rsttherm", "measure", "-p", "128,96", "-e", "0.98"],
            vec![
                "rsttherm",
                "measure",
                "--reflected",
                "-5",
                "--offset",
                "-1.5",
            ],
            vec!["rsttherm", "-d", "P3", "shutter"],
            vec!["rsttherm", "serve", "--port", "9000", "--bind", "0.0.0.0"],
        ] {
            assert!(Cli::try_parse_from(&args).is_ok(), "{args:?} should parse");
        }
    }

    /// Everything here exits 8, since `main` maps any parse error to EXIT_USAGE.
    #[test]
    fn rejects_bad_syntax_and_out_of_range_values() {
        for args in [
            vec!["rsttherm"],
            vec!["rsttherm", "sideways"],
            vec!["rsttherm", "measure", "-e", "0"],
            vec!["rsttherm", "measure", "-e", "1.5"],
            vec!["rsttherm", "measure", "-p", "128"],
            vec!["rsttherm", "-d", "3474:zzzz", "info"],
            vec!["rsttherm", "-d", "", "info"],
        ] {
            let Err(err) = Cli::try_parse_from(&args) else {
                panic!("{args:?} should fail")
            };
            assert!(err.use_stderr(), "{args:?} should be a usage error");
        }
    }

    #[test]
    fn help_and_version_are_not_failures() {
        for args in [vec!["rsttherm", "--help"], vec!["rsttherm", "--version"]] {
            let Err(err) = Cli::try_parse_from(&args) else {
                panic!("{args:?} exits via Err")
            };
            assert!(!err.use_stderr(), "{args:?} should exit 0");
        }
    }
}
