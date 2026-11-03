//! Pure meteogram domain logic.
//!
//! Acquisition is performed by a device/network worker. This module owns the
//! validated request description, bounded response decoding, weather-to-pixel
//! rendering, and the foreground program state machine. It never touches
//! hardware or a clock.

mod bitmap;
mod font;
mod forecast;
mod location;
mod program;
mod render;

pub use bitmap::{
    BlackRuns, Meteogram, CANVAS_HEIGHT, CANVAS_WIDTH, VIEWPORT_HEIGHT, VIEWPORT_WIDTH,
};
pub use forecast::{decode_forecast, Forecast, ForecastError, ForecastRequest, MAX_RESPONSE_BYTES};
pub use location::{default_location, Location, LocationError};
pub use program::{Effect, Event, MeteoProgram, ProgramState, RequestId};
pub use render::{render, RenderError};
