//! Temporary bridge from the current blocking shell to the future meteogram
//! program contract. All weather parsing, rendering, and state transitions
//! remain in `dynatac-core`; this file only performs device I/O.

use std::thread::{self, sleep};
use std::time::Duration;
use std::{io, io::Read};

use dynatac_core::meteo::{
    decode_forecast, default_location, render, Effect, Event, ForecastRequest, MeteoProgram,
    ProgramState, RequestId,
};
use dynatac_core::modem::Modem;
use dynatac_core::network::{ensure_connectivity, APN};
use dynatac_core::wifi::WifiDriver;
use esp_idf_svc::http::client::{Configuration, EspHttpConnection};
use esp_idf_svc::http::Method;

use crate::display::Epd;
use crate::keyboard::Keyboard;

const BODY_X: u16 = 4;
const BODY_Y: u16 = 18;
// Serde's generated visitor for the wide Meteoblue hourly object needs more
// stack than the current 8,000-byte ESP-IDF main task has. Keep that cost on
// the worker, whose stack is released as soon as the bitmap has been produced.
const WORKER_STACK_BYTES: usize = 32 * 1024;

pub fn run(
    epd: &mut Epd<'_>,
    keyboard: &mut Keyboard<'_, '_>,
    wifi: &mut dyn WifiDriver,
    modem: &mut dyn Modem,
    api_key: &str,
) -> Result<(), String> {
    ensure_connectivity(wifi, modem, APN)
        .map_err(|error| format!("no connectivity: {}", error.display()))?;

    let location = default_location();
    let worker_location = location.clone();
    let (mut program, _) = MeteoProgram::start(location, RequestId(1));
    let worker_key = api_key.to_owned();
    let worker = thread::Builder::new()
        .name("meteo-fetch".into())
        .stack_size(WORKER_STACK_BYTES)
        .spawn(move || {
            let request = ForecastRequest::new(worker_location);
            fetch_and_render(&request, &worker_key)
        })
        .map_err(|error| format!("could not start meteo worker: {error}"))?;
    let result = worker
        .join()
        .map_err(|_| "meteo worker panicked".to_string())?;
    program.update(Event::Completed {
        request_id: RequestId(1),
        result,
    });

    match program.state() {
        ProgramState::Failed { message } => return Err(message.clone()),
        ProgramState::Ready { .. } => draw_program(epd, &program)?,
        ProgramState::Loading { .. } => return Err("meteo remained in loading state".into()),
    }

    loop {
        match keyboard.poll() {
            Ok(Some(key)) => match program.handle_key(key) {
                Some(Effect::Redraw) => draw_program(epd, &program)?,
                Some(Effect::Exit) => return Ok(()),
                Some(Effect::Fetch { .. }) | None => {}
            },
            Ok(None) => {}
            Err(error) => return Err(format!("keyboard: {error:?}")),
        }
        sleep(Duration::from_millis(10));
    }
}

fn fetch_and_render(
    request: &ForecastRequest,
    api_key: &str,
) -> Result<dynatac_core::meteo::Meteogram, String> {
    let url = request.url(api_key).map_err(|error| error.to_string())?;
    let config = Configuration {
        timeout: Some(Duration::from_secs(30)),
        crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
        ..Default::default()
    };
    let mut connection =
        EspHttpConnection::new(&config).map_err(|_| "could not create HTTPS client".to_string())?;
    connection
        .initiate_request(Method::Get, &url, &[("Accept", "application/json")])
        .map_err(|_| "could not start Meteoblue request".to_string())?;
    connection
        .initiate_response()
        .map_err(|_| "Meteoblue request failed".to_string())?;
    let status = connection.status();
    if !(200..300).contains(&status) {
        return Err(format!("Meteoblue returned HTTP {status}"));
    }
    let forecast = decode_forecast(ConnectionReader(&mut connection), request)
        .map_err(|error| error.to_string())?;
    render(&forecast).map_err(|error| error.to_string())
}

/// `EspHttpConnection` uses embedded-io on the ESP target. The pure decoder
/// intentionally accepts `std::io::Read`, so this tiny adapter is the complete
/// platform boundary.
struct ConnectionReader<'a>(&'a mut EspHttpConnection);

impl Read for ConnectionReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0
            .read(buffer)
            .map_err(|error| io::Error::new(io::ErrorKind::Other, format!("{error:?}")))
    }
}

fn draw_program(epd: &mut Epd<'_>, program: &MeteoProgram) -> Result<(), String> {
    let ProgramState::Ready { bitmap, scroll_x } = program.state() else {
        return Err("meteo is not ready to draw".into());
    };
    epd.fb.clear_all_desired();
    // Minimal kernel-owned status-bar stand-in. The real kernel supplies its
    // own status surface; the meteogram only draws below BODY_Y.
    epd.fb.fill_rect(0, 13, 240, 1, true);
    for (x, y, width) in bitmap.black_runs(*scroll_x) {
        epd.fb.fill_rect(BODY_X + x, BODY_Y + y, width, 1, true);
    }
    epd.try_flush()
        .map(|_| ())
        .map_err(|error| format!("display: {error:?}"))
}
